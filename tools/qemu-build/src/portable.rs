// qemu-build: custom QEMU runtime packaging
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Builds macOS and Windows QEMU from pinned sources with static dependencies.

use std::fs;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{Build, Target, bundle, output, run};

/// Compile pinned dependency sources and the selected system emulator.
pub(crate) fn build(build: &Build, jobs: usize) -> Result<()> {
    let definitions: Vec<Value> = serde_json::from_slice(&fs::read(
        build
            .repo
            .join(".github/packaging/qemu/portable-sources.json"),
    )?)?;
    fs::create_dir_all(build.output.join("sources/dependencies"))?;
    for definition in &definitions {
        fetch(build, definition)?;
    }

    // Scope every dependency lookup to our prefix, including cross compilation
    fs::copy(
        build.repo.join(".github/packaging/qemu/windows.cross"),
        build.output.join("windows.cross"),
    )?;
    dependencies(build, jobs)?;
    let cpu = build.target.cpu();
    let sources = build.output.join("sources/qemu");
    fs::copy(
        build.repo.join(format!(
            ".github/packaging/qemu/{}.mak",
            build.target.arch()
        )),
        sources.join(format!("configs/devices/{cpu}-softmmu/ark.mak")),
    )?;
    fs::create_dir_all(build.output.join("build"))?;

    // The common feature list keeps transports and disk behavior aligned
    let common = fs::read_to_string(build.repo.join(".github/packaging/qemu/linux.args"))?;
    let mut args: Vec<String> = common
        .lines()
        .filter(|line| {
            !line.starts_with("--target-list=")
                && !line.starts_with("--with-devices-")
                && *line != "--enable-kvm"
        })
        .map(str::to_owned)
        .collect();
    args.extend([
        format!("--target-list={cpu}-softmmu"),
        format!("--with-devices-{cpu}=ark"),
    ]);
    if build.target == Target::WindowsAmd64 {
        args.extend(
            [
                "--cross-prefix=x86_64-w64-mingw32-",
                "--enable-whpx",
                "--static",
                "--disable-pie",
                "--extra-cflags=-DLIBSLIRP_STATIC",
            ]
            .map(str::to_owned),
        );
    } else {
        args.push("--enable-hvf".to_owned());
        // Apple's system libraries stay dynamic; pkg-config supplies archive dependencies
        let libraries = output(build.container("/work")?.args([
            "pkg-config",
            "--static",
            "--libs",
            "glib-2.0",
            "gmodule-no-export-2.0",
            "pixman-1",
            "slirp",
            "zlib",
        ]))?;
        args.push(format!("--extra-ldflags={libraries}"));
    }
    if build.target.arch() == "arm64" {
        args.push("--enable-fdt".to_owned());
    }
    fs::write(
        build.output.join("configure.json"),
        serde_json::to_vec_pretty(&args)?,
    )?;
    run(build
        .container("/work/build")?
        .arg(build.work_path("sources/qemu/configure"))
        .args(&args))?;
    run(build.container("/work/build")?.args([
        "ninja",
        "-j",
        &jobs.to_string(),
        &format!("qemu-system-{cpu}{}", build.target.extension()),
        &format!("qemu-img{}", build.target.extension()),
    ]))
}

/// Download and verify an upstream release before extracting it into the build area.
fn fetch(build: &Build, definition: &Value) -> Result<()> {
    let name = field(definition, "name")?;
    let hash = field(definition, "sha256")?;
    let archive = build
        .output
        .join(format!("sources/dependencies/{name}.tar"));
    if !archive.exists() {
        let partial = archive.with_extension("partial");
        run(Command::new("curl")
            .args(["--fail", "--location", "--retry", "3", "--output"])
            .arg(&partial)
            .arg(field(definition, "url")?))?;
        ensure!(
            bundle::digest(&fs::read(&partial)?) == hash,
            "source checksum mismatch for {name}"
        );
        fs::rename(partial, &archive)?;
    }
    ensure!(
        bundle::digest(&fs::read(&archive)?) == hash,
        "cached source mismatch for {name}; use a fresh QEMU_OUTPUT directory"
    );
    let destination = build.output.join("dependencies").join(name);
    let marker = destination.join(".ark-source-sha256");
    if marker.is_file() {
        ensure!(
            fs::read_to_string(marker)? == hash,
            "extracted source mismatch for {name}"
        );
        return Ok(());
    }
    fs::create_dir_all(&destination)?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("--strip-components=1")
        .arg("-C")
        .arg(&destination))?;
    fs::write(marker, hash)?;
    Ok(())
}

/// Read a required string from the checked-in dependency lock.
fn field<'a>(definition: &'a Value, name: &str) -> Result<&'a str> {
    definition[name]
        .as_str()
        .with_context(|| format!("source definition is missing {name}"))
}

/// Build the small library set without adding package-manager runtime dependencies.
fn dependencies(build: &Build, jobs: usize) -> Result<()> {
    let prefix = build.work_path("prefix");
    let windows = build.target == Target::WindowsAmd64;
    fs::create_dir_all(build.output.join("prefix"))?;
    fs::create_dir_all(build.output.join("dependency-build"))?;

    // QEMU's WHPX capabilities need newer SDK declarations than Ubuntu ships
    if windows {
        fs::create_dir_all(build.output.join("prefix/include"))?;
        for header in ["winhvplatform.h", "winhvplatformdefs.h"] {
            fs::copy(
                build
                    .output
                    .join("dependencies/mingw-w64/mingw-w64-headers/include")
                    .join(header),
                build.output.join("prefix/include").join(header),
            )?;
        }
    }

    // CMake installs only static compression and regular expression libraries
    for (name, options) in [
        (
            "zlib",
            vec![
                "-DZLIB_BUILD_SHARED=OFF",
                "-DZLIB_BUILD_STATIC=ON",
                "-DZLIB_BUILD_TESTING=OFF",
            ],
        ),
        (
            "pcre2",
            vec![
                "-DBUILD_SHARED_LIBS=OFF",
                "-DPCRE2_BUILD_TESTS=OFF",
                "-DPCRE2_BUILD_PCRE2GREP=OFF",
                "-DPCRE2_SUPPORT_JIT=OFF",
            ],
        ),
    ] {
        let directory = format!("dependency-build/{name}");
        let mut command = build.container("/work")?;
        command.args([
            "cmake",
            "-G",
            "Ninja",
            "-S",
            &build.work_path(&format!("dependencies/{name}")),
            "-B",
            &build.work_path(&directory),
            "-DCMAKE_BUILD_TYPE=MinSizeRel",
            &format!("-DCMAKE_INSTALL_PREFIX={prefix}"),
            "-DCMAKE_INSTALL_LIBDIR=lib",
        ]);
        if windows {
            command.args([
                "-DCMAKE_SYSTEM_NAME=Windows",
                "-DCMAKE_C_COMPILER=x86_64-w64-mingw32-gcc",
                "-DCMAKE_RC_COMPILER=x86_64-w64-mingw32-windres",
            ]);
        }
        run(command.args(options))?;
        run(build.container("/work")?.args([
            "cmake",
            "--build",
            &build.work_path(&directory),
            "--parallel",
            &jobs.to_string(),
        ]))?;
        run(build
            .container("/work")?
            .args(["cmake", "--install", &build.work_path(&directory)]))?;
        if windows && name == "zlib" {
            // zlib's Windows archive has an 's' suffix that its pkg-config file omits
            fs::copy(
                build.output.join("prefix/lib/libzs.a"),
                build.output.join("prefix/lib/libz.a"),
            )?;
        }
    }

    // GLib configures its object library too, which needs libffi at build time
    fs::create_dir_all(build.output.join("dependency-build/libffi"))?;
    let mut configure = build.container("/work/dependency-build/libffi")?;
    configure
        .arg(build.work_path("dependencies/libffi/configure"))
        .args([
            format!("--prefix={prefix}"),
            "--disable-shared".to_owned(),
            "--enable-static".to_owned(),
            "--disable-docs".to_owned(),
            "--disable-multi-os-directory".to_owned(),
        ]);
    if windows {
        configure.arg("--host=x86_64-w64-mingw32");
    }
    run(&mut configure)?;
    run(build.container("/work/dependency-build/libffi")?.args([
        "make",
        "-j",
        &jobs.to_string(),
        "install",
    ]))?;

    // libslirp declares GNU iconv as a Windows dependency
    if windows {
        fs::create_dir_all(build.output.join("dependency-build/libiconv"))?;
        run(build
            .container("/work/dependency-build/libiconv")?
            .arg(build.work_path("dependencies/libiconv/configure"))
            .args([
                format!("--prefix={prefix}"),
                "--host=x86_64-w64-mingw32".to_owned(),
                "--disable-shared".to_owned(),
                "--enable-static".to_owned(),
                "--disable-nls".to_owned(),
            ]))?;
        run(build.container("/work/dependency-build/libiconv")?.args([
            "make",
            "-j",
            &jobs.to_string(),
            "install",
        ]))?;
    }

    // The translation shim avoids introducing a gettext runtime into QEMU
    bundle::copy_tree(
        &build.output.join("dependencies/proxy-libintl"),
        &build
            .output
            .join("dependencies/glib/subprojects/proxy-libintl-0.5"),
    )?;
    for (name, options) in [
        (
            "glib",
            vec![
                "-Dtests=false",
                "-Dinstalled_tests=false",
                "-Dnls=disabled",
                "-Ddocumentation=false",
                "-Dintrospection=disabled",
                "--force-fallback-for=intl",
            ],
        ),
        (
            "pixman",
            vec![
                "-Dtests=disabled",
                "-Ddemos=disabled",
                "-Dgtk=disabled",
                "-Dlibpng=disabled",
            ],
        ),
        ("libslirp", vec![]),
    ] {
        let directory = format!("dependency-build/{name}");
        let mut command = build.container("/work")?;
        command.args([
            "python3",
            &build.work_path("dependencies/meson/meson.py"),
            "setup",
            &build.work_path(&directory),
            &build.work_path(&format!("dependencies/{name}")),
            &format!("--prefix={prefix}"),
            "--libdir=lib",
            "--buildtype=minsize",
            "--default-library=static",
            "--auto-features=disabled",
            "--wrap-mode=nodownload",
        ]);
        if windows {
            command.args(["--cross-file", "/work/windows.cross"]);
            if name == "libslirp" {
                command.arg("-Dstatic=true");
            }
        }
        if build
            .output
            .join(&directory)
            .join("meson-private/coredata.dat")
            .exists()
        {
            command.arg("--reconfigure");
        }
        run(command.args(options))?;
        let mut compile = build.container("/work")?;
        compile.args([
            "ninja",
            "-C",
            &build.work_path(&directory),
            "-j",
            &jobs.to_string(),
        ]);
        if name == "glib" {
            // QEMU uses GLib and GModule; GIO also requires a newer Windows SDK
            run(compile.args([
                "glib/libglib-2.0.a",
                "gmodule/libgmodule-2.0.a",
                "subprojects/proxy-libintl-0.5/libintl.a",
            ]))?;
            install_glib(build)?;
        } else {
            run(compile.arg("install"))?;
            if name == "libslirp" && windows {
                // libslirp's uninstalled thin archive refers to objects beside it
                let members = output(build.container("/work/dependency-build/libslirp")?.args([
                    "x86_64-w64-mingw32-ar",
                    "t",
                    "libslirp.a",
                ]))?;
                let archive = build.output.join("prefix/lib/libslirp.a");
                if archive.exists() {
                    fs::remove_file(archive)?;
                }
                run(build
                    .container("/work/dependency-build/libslirp")?
                    .args([
                        "x86_64-w64-mingw32-ar",
                        "rcs",
                        "/work/prefix/lib/libslirp.a",
                    ])
                    .args(members.lines()))?;
            }
        }
    }
    Ok(())
}

/// Install GLib's selected archives and public headers from Meson's install plan.
fn install_glib(build: &Build) -> Result<()> {
    let plan: Value = serde_json::from_slice(&fs::read(
        build
            .output
            .join("dependency-build/glib/meson-info/intro-install_plan.json"),
    )?)?;
    let prefix = build.work_path("prefix");
    for group in plan
        .as_object()
        .context("invalid GLib install plan")?
        .values()
    {
        for (source, item) in group.as_object().context("invalid GLib install group")? {
            let destination = item["destination"]
                .as_str()
                .context("missing GLib install destination")?
                .replace("{prefix}", &prefix)
                .replace("{libdir_static}", &format!("{prefix}/lib"))
                .replace("{libdir}", &format!("{prefix}/lib"))
                .replace("{includedir}", &format!("{prefix}/include"));
            let Some(relative) = destination.strip_prefix(&format!("{prefix}/")) else {
                continue;
            };
            let wanted = relative.starts_with("include/glib-2.0/glib/")
                || relative.starts_with("include/glib-2.0/gmodule/")
                || matches!(
                    relative,
                    "include/glib-2.0/glib.h"
                        | "include/glib-2.0/glib-unix.h"
                        | "include/glib-2.0/gmodule.h"
                        | "include/libintl.h"
                        | "lib/glib-2.0/include/glibconfig.h"
                        | "lib/libglib-2.0.a"
                        | "lib/libgmodule-2.0.a"
                        | "lib/libintl.a"
                        | "lib/pkgconfig/glib-2.0.pc"
                        | "lib/pkgconfig/gmodule-2.0.pc"
                        | "lib/pkgconfig/gmodule-no-export-2.0.pc"
                        | "lib/pkgconfig/gmodule-export-2.0.pc"
                );
            if wanted {
                let source = std::path::Path::new(source).strip_prefix(build.work_path(""))?;
                let source = build.output.join(source);
                let target = build.output.join("prefix").join(relative);
                fs::create_dir_all(target.parent().context("invalid install path")?)?;
                // Preserve header timestamps so unchanged installs do not rebuild QEMU
                if !target.is_file() || fs::read(&source)? != fs::read(&target)? {
                    fs::copy(source, target)?;
                }
            }
        }
    }
    Ok(())
}

/// Collect static executables, source provenance, ROMs and license notices.
pub(crate) fn package(build: &Build) -> Result<()> {
    let runtime = build.output.join("runtime");
    if runtime.exists() {
        fs::remove_dir_all(&runtime)?;
    }
    let libs = runtime.join("qemu-libs");
    let licenses = libs.join("licenses");
    fs::create_dir_all(runtime.join("binaries"))?;
    fs::create_dir_all(&licenses)?;
    for (source, name) in [
        (
            format!("qemu-system-{}", build.target.cpu()),
            "qemu-system-guest",
        ),
        ("qemu-img".to_owned(), "qemu-img"),
    ] {
        let filename = format!(
            "{name}-{}{}",
            build.target.triple(),
            build.target.extension()
        );
        fs::copy(
            build
                .output
                .join("build")
                .join(format!("{source}{}", build.target.extension())),
            runtime.join("binaries").join(&filename),
        )?;
        let mut command = build.container("/work/runtime/binaries")?;
        if build.target.is_macos() {
            command.args(["strip", "-x"]);
        } else {
            command.args(["x86_64-w64-mingw32-strip", "--strip-unneeded"]);
        }
        run(command.arg(&filename))?;
        if build.target.is_macos() {
            run(Command::new("codesign")
                .args([
                    "--force",
                    "--sign",
                    "-",
                    "--timestamp=none",
                    "--entitlements",
                ])
                .arg(
                    build
                        .output
                        .join("sources/qemu/accel/hvf/entitlements.plist"),
                )
                .arg(runtime.join("binaries").join(&filename)))?;
        }
    }

    // Keep notices for all dependency sources, including code incorporated statically
    let definitions: Vec<Value> = serde_json::from_slice(&fs::read(
        build
            .repo
            .join(".github/packaging/qemu/portable-sources.json"),
    )?)?;
    for definition in &definitions {
        let name = field(definition, "name")?;
        for license in definition["licenses"]
            .as_array()
            .context("missing license paths")?
        {
            let path = license.as_str().context("invalid license path")?;
            fs::copy(
                build.output.join("dependencies").join(name).join(path),
                licenses.join(format!("{name}-{}", path.replace('/', "-"))),
            )?;
        }
    }
    for (name, path) in [
        ("QEMU-LICENSE", "LICENSE"),
        ("QEMU-COPYING", "COPYING"),
        ("QEMU-COPYING.LIB", "COPYING.LIB"),
        ("SeaBIOS", "roms/seabios/COPYING"),
        ("SeaBIOS-LGPL", "roms/seabios/COPYING.LESSER"),
        ("iPXE", "roms/ipxe/COPYING"),
        ("iPXE-GPL", "roms/ipxe/COPYING.GPLv2"),
        ("iPXE-UBDL", "roms/ipxe/COPYING.UBDL"),
        ("EDK2", "roms/edk2/License.txt"),
        ("keycodemapdb", "subprojects/keycodemapdb/LICENSE.BSD"),
        ("keycodemapdb-GPL", "subprojects/keycodemapdb/LICENSE.GPL2"),
    ] {
        fs::copy(
            build.output.join("sources/qemu").join(path),
            licenses.join(name),
        )?;
    }
    if build.target.arch() == "arm64" {
        fs::copy(
            build
                .output
                .join("sources/qemu/subprojects/dtc/BSD-2-Clause"),
            licenses.join("libfdt"),
        )?;
    }
    if build.target == Target::WindowsAmd64 {
        run(build.container("/work")?.args([
            "cp",
            "-RL",
            "/usr/share/common-licenses",
            "/work/runtime/qemu-libs/licenses/common",
        ]))?;
        for package in ["mingw-w64-common", "gcc-mingw-w64-x86-64-posix"] {
            let text = output(
                build
                    .container("/work")?
                    .args(["cat", &format!("/usr/share/doc/{package}/copyright")]),
            )?;
            fs::write(licenses.join(format!("{package}.copyright")), text)?;
        }
    }
    for rom in fs::read_to_string(build.repo.join(format!(
        ".github/packaging/qemu/{}.roms",
        build.target.arch()
    )))?
    .lines()
    {
        fs::copy(
            build.output.join("sources/qemu/pc-bios").join(rom),
            libs.join(rom),
        )?;
    }
    fs::write(
        libs.join("THIRD_PARTY_NOTICES.txt"),
        format!(
            "This runtime contains QEMU and statically linked libraries identified in manifest.json.\nTheir notices are in licenses/. Their licenses remain applicable independently of\nthe emulator's BSD license. Complete corresponding sources and rebuild recipes\naccompany this release as *-{}-qemu-sources.tar.xz at\nhttps://github.com/dark-bio/emulator/releases\n",
            build.target.label()
        ),
    )?;

    // Archive the same inputs used for the executables and their dependency checks
    let origins: Value = serde_json::from_slice(&fs::read(build.output.join("origins.json"))?)?;
    let mut configure: Value =
        serde_json::from_slice(&fs::read(build.output.join("configure.json"))?)?;
    if let Some(args) = configure.as_array_mut() {
        for arg in args {
            if let Some(value) = arg.as_str() {
                *arg = Value::String(
                    value.replace(build.output.to_string_lossy().as_ref(), "${QEMU_OUTPUT}"),
                );
            }
        }
    }
    let toolchain = if build.target.is_macos() {
        json!({"compiler":output(Command::new("clang").arg("--version"))?, "sdk":output(Command::new("xcrun").arg("--show-sdk-version"))?})
    } else {
        json!({"image":build.image, "image_id":output(Command::new("docker").args(["image", "inspect", "--format", "{{.Id}}", &build.image]))?, "apt_mirror_override":build.apt_mirror})
    };
    let manifest = json!({"target":build.target.label(), "qemu":fs::read_to_string(build.output.join("sources/qemu/VERSION"))?.trim(), "origins":origins, "dependencies":definitions, "configure":configure, "toolchain":toolchain, "files":bundle::inventory(&runtime)?, "linkage":"static dependencies; operating system libraries remain dynamic"});
    fs::write(
        libs.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    bundle::source_archive(build)?;
    let artifacts = build.output.join("artifacts");
    let name = format!("{}-qemu-runtime.tar.xz", build.target.label());
    run(Command::new("tar")
        .arg("-cJf")
        .arg(artifacts.join(&name))
        .arg("-C")
        .arg(&runtime)
        .arg("."))?;
    let mut checksums = String::new();
    for name in [
        name,
        format!("{}-qemu-sources.tar.xz", build.target.label()),
    ] {
        checksums.push_str(&format!(
            "{}  {name}\n",
            bundle::digest(&fs::read(artifacts.join(&name))?)
        ));
    }
    fs::write(artifacts.join("SHA256SUMS"), checksums)?;
    Ok(())
}

/// Verify artifact integrity, system-only linkage and native runtime behavior.
pub(crate) fn check(build: &Build) -> Result<()> {
    crate::portable_check::check(build)
}
