FROM public.ecr.aws/docker/library/ubuntu:24.04@sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates

ARG APT_MIRROR=https://snapshot.ubuntu.com/ubuntu/20261008T000000Z
RUN --mount=type=cache,id=ark-qemu-windows-apt,target=/var/cache/apt,sharing=locked \
    rm -f /etc/apt/sources.list.d/ubuntu.sources /etc/apt/apt.conf.d/docker-clean \
    && printf '%s\n' \
    "deb $APT_MIRROR noble main universe" \
    "deb $APT_MIRROR noble-updates main universe" \
    "deb $APT_MIRROR noble-security main universe" \
    > /etc/apt/sources.list \
    && apt-get -o APT::Update::Error-Mode=any -o Acquire::Retries=5 -o Acquire::Check-Valid-Until=false update \
    && DEBIAN_FRONTEND=noninteractive apt-get -o Acquire::Retries=5 install -y --no-install-recommends \
    build-essential git cmake ninja-build pkg-config python3 python3-venv \
    python3-setuptools python3-wheel python3-packaging xz-utils \
    gcc-mingw-w64-x86-64-posix g++-mingw-w64-x86-64-posix

ENV LC_ALL=C.UTF-8
