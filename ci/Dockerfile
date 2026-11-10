FROM docker.io/archlinux/archlinux@sha256:4aa51c8fd4b544a76661a96f39e983e01129e9a2526061e5a0e46c38fd57e6c5 AS transactions

# The database digests pin the complete package graph. Pacman also verifies package signatures.
RUN printf '%s\n' 'Server = https://archive.archlinux.org/repos/2026/11/03/$repo/os/$arch' > /etc/pacman.d/mirrorlist \
    && pacman -Sy --noconfirm \
    && printf '%s\n' \
        'd878496b074f4d0e946de0216d0fe00f5bc70ac34bbb16025bd57191affce57e  /var/lib/pacman/sync/core.db' \
        '1848ec50754657edf473b42680099511ba244671f9e008d780e7c82e33911198  /var/lib/pacman/sync/extra.db' \
        | sha256sum --check \
    && pacman -Su --noconfirm --needed attr btrfs-progs bubblewrap curl dosfstools git jq libseccomp \
        mtools openssh openssl python ripgrep sqlite systemd util-linux \
    && rm -f /var/cache/pacman/pkg/*

COPY rust-dist.sha256 /tmp/rust-dist.sha256
RUN set -eu; \
    cd /tmp; \
    while read -r checksum archive; do \
        curl --fail --silent --show-error --location --proto '=https' --retry 3 \
            --output "$archive" "https://static.rust-lang.org/dist/2026-10-15/$archive"; \
        printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check; \
        tar -xf "$archive"; \
        "./${archive%.tar.xz}/install.sh" --prefix=/opt/cps-rust --disable-ldconfig; \
        rm -rf -- "$archive" "${archive%.tar.xz}"; \
    done < rust-dist.sha256; \
    rm rust-dist.sha256
ENV PATH="/opt/cps-rust/bin:${PATH}"

# tests/deploy-mirror.sh signs TUF roots with tuftool, built from its published lock file.
RUN cargo install --locked --root /opt/cps-tools tuftool@0.17.0 \
    && rm -rf /root/.cargo
ENV PATH="/opt/cps-tools/bin:${PATH}"

FROM transactions AS boot
RUN pacman -S --noconfirm --needed bc cmake cpio edk2-ovmf elfutils kmod \
        mkinitcpio-busybox qemu-system-x86 \
    && rm -f /var/cache/pacman/pkg/*
