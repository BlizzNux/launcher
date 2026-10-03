# Build environment for the release bundles.
# An AppImage carries the build machine's libraries, and those only load on systems whose
# glibc is at least as new. Releases are therefore built on an old LTS base instead of the
# developer's machine. Used by scripts/container-build.sh.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl file git patchelf pkg-config wget xz-utils xdg-utils \
        libwebkit2gtk-4.1-dev libjavascriptcoregtk-4.1-dev libsoup-3.0-dev libgtk-3-dev \
        libayatana-appindicator3-dev librsvg2-dev libssl-dev libxdo-dev \
    && rm -rf /var/lib/apt/lists/*

# Node (for the Tauri CLI in node_modules): the current 22.x LTS build from nodejs.org.
RUN f=$(curl -fsSL https://nodejs.org/dist/latest-v22.x/SHASUMS256.txt | awk '/linux-x64.tar.xz/{print $2}') \
    && curl -fsSL "https://nodejs.org/dist/latest-v22.x/$f" | tar -xJ -C /usr/local --strip-components=1

# Rust: the toolchain lives in the image; the registry and build cache are mounted at run time.
ENV RUSTUP_HOME=/opt/rustup PATH=/opt/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | CARGO_HOME=/opt/cargo sh -s -- -y --profile minimal --default-toolchain stable \
    && chmod -R a+rwX /opt/rustup /opt/cargo
