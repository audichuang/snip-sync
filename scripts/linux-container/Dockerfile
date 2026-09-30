# The Linux side of `just preflight` on hosts that are not Linux: what CI's
# ubuntu-24.04 jobs install (.github/workflows/ci.yml), and nothing more.
# Add a package here when a CI job gains one.
FROM ubuntu:24.04

ARG ACTIONLINT_VERSION=1.7.12
# CI's setup-node major; the TS reference needs node:module registerHooks.
ARG NODE_MAJOR=24
ENV DEBIAN_FRONTEND=noninteractive

# Docs, man pages and translations are never read here. Only lavapipe renders
# in a VM, so the hardware Vulkan drivers go.
RUN printf 'path-exclude=/usr/share/%s/*\n' doc man info locale > /etc/dpkg/dpkg.cfg.d/01-slim \
    && apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl git pkg-config xz-utils \
        python3 python3-pil shellcheck \
        xvfb xdotool xclip x11-apps x11-utils imagemagick dbus dbus-x11 \
        fcitx5 fcitx5-pinyin \
        libxcb1-dev libx11-dev libxkbcommon-dev libxkbcommon-x11-dev libxkbcommon-x11-0 \
        libfontconfig1-dev libfreetype6-dev libwayland-dev mesa-vulkan-drivers \
        fonts-dejavu-core fonts-noto-cjk \
    && find /usr/lib -name 'libvulkan_*.so' ! -name libvulkan_lvp.so -delete \
    && find /usr/share/vulkan/icd.d -type f ! -name 'lvp_icd*' -delete \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN arch="$(dpkg --print-architecture)" \
    && curl -sSfL "https://github.com/rhysd/actionlint/releases/download/v${ACTIONLINT_VERSION}/actionlint_${ACTIONLINT_VERSION}_linux_${arch}.tar.gz" \
        | tar -xz -C /usr/local/bin actionlint \
    && node_arch="$(echo "$arch" | sed 's/amd64/x64/')" \
    && ver="$(curl -sSfL https://nodejs.org/dist/index.json | python3 -c 'import json, sys; print(next(r["version"] for r in json.load(sys.stdin) if r["version"].startswith(f"v{sys.argv[1]}.")))' "$NODE_MAJOR")" \
    && curl -sSfL "https://nodejs.org/dist/${ver}/node-${ver}-linux-${node_arch}.tar.xz" \
        | tar -xJ -C /usr/local/bin --strip-components=2 "node-${ver}-linux-${node_arch}/bin/node" \
    && curl -sSf https://just.systems/install.sh | bash -s -- --to /usr/local/bin \
    && curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path \
        --default-toolchain stable --component rustfmt,clippy \
    && rm -rf /opt/rustup/toolchains/*/share/doc /opt/rustup/downloads /opt/rustup/tmp \
    && chown -R ubuntu:ubuntu /opt/cargo

# The checkout is bind-mounted and owned by the host user; tests commit into fixture repos.
RUN git config --system --add safe.directory '*' \
    && git config --system user.name snip-ci \
    && git config --system user.email snip-ci@example.invalid

COPY entrypoint.sh /usr/local/bin/snip-entrypoint
ENV SNIP_NATIVE_PYTHON=/usr/bin/python3
ENTRYPOINT ["/usr/local/bin/snip-entrypoint"]
