FROM public.ecr.aws/docker/library/ubuntu:22.04@sha256:5ec03bb3441e8b0bf3b4f9cd4629a1ae763010dc3035bb8da3ae6cf026486401

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates

# The snapshot fixes compiler and library inputs while retaining the glibc 2.35 floor.
RUN printf '%s\n' \
    'deb http://snapshot.ubuntu.com/ubuntu/20261008T000000Z jammy main universe' \
    'deb http://snapshot.ubuntu.com/ubuntu/20261008T000000Z jammy-updates main universe' \
    'deb http://snapshot.ubuntu.com/ubuntu/20261008T000000Z jammy-security main universe' \
    'deb-src http://snapshot.ubuntu.com/ubuntu/20261008T000000Z jammy main universe' \
    'deb-src http://snapshot.ubuntu.com/ubuntu/20261008T000000Z jammy-updates main universe' \
    'deb-src http://snapshot.ubuntu.com/ubuntu/20261008T000000Z jammy-security main universe' \
    > /etc/apt/sources.list \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential ca-certificates git ninja-build pkg-config python3 python3-venv \
    python3-tomli python3-wheel \
    libglib2.0-dev libpixman-1-dev libslirp-dev zlib1g-dev xz-utils

ENV LC_ALL=C.UTF-8
