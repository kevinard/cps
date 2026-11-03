# Canopa Package System

This Rust prototype manages disposable Linux directory images from a signed local repository. It stores immutable package manifests and SHA-256 file payloads, plans changes across the image, and verifies installed files offline.

## Build and try it

Build with `cargo build`. The prototype needs Linux with `openat2`, `/proc`, and the SQLite development library. The example uses OpenSSL to generate a local Ed25519 signing key. Run it as your regular user, with package UID and GID matching that user.

```sh
cps="$PWD/target/debug/cps"
work=$(mktemp -d)
cd "$work"
mkdir -p proto/usr/share/cps-demo
printf 'Hello from Canopa\n' > proto/usr/share/cps-demo/hello

openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

cat > hello.kdl <<MANIFEST
schema-version 1
package "demo/hello" publisher="canopa" version="1.0.0-0" {
    file "usr/share/cps-demo/hello" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST

cat > image.kdl <<'CONFIG'
schema-version 1
publisher "canopa" {
    repository "repo"
    trust-key "public.der"
}
CONFIG

"$cps" publish --repo repo --proto proto --signing-key private.der hello.kdl
"$cps" image-create --root image --config image.kdl
"$cps" plan --root image --output install.json install demo/hello
"$cps" apply --root image install.json
"$cps" verify --root image
```

To upgrade, publish the revised recipe under a new version and run `cps plan --root image --output upgrade.json update`, then apply the saved plan. Each plan pins a signed catalog and an image revision. Keep the saved plan until apply finishes. If interrupted, rerun `apply` with the same plan. Once writes begin, recovery uses the payloads staged inside the image and can finish offline.

## Manifest and image behavior

- Manifests and image configuration use KDL 2 and begin with `schema-version 1`. Paths in image configuration resolve relative to the configuration file. It also accepts `architecture "x86_64"` and `facet "doc.man" #false`. Convert old TOML recipes before publishing new versions. Existing JSON manifest objects stay readable under their original digests.
- Versions have the numeric form `major.minor.patch-revision`. Inside a package block, declare dependencies as `require "library/libdisplay" version=">=1.0.0-0, <2.0.0-0"` or `incorporate "library/libdisplay" version="=1.0.0-0"`. Only `require` pulls in an absent package.
- Actions support `file`, `dir`, and `symlink`. A symlink specifies `target` in place of `mode`. The optional `arch` and `facet` selectors match image policy. Facets default to enabled and can be set with `image-create --facet doc.man=false`.
- Files marked `preserve=#true` keep local edits through an upgrade, including a package ownership transfer at the same path. When the vendor file changes, find its new copy in `image/.cps/config-updates/<revision>/`.
- The image must be inactive and reside on one filesystem. The prototype rejects directory metadata changes and transitions between a directory and a file or symlink. Directories left behind when their ownership ends are retained.

Run `cargo test` for the desktop upgrade and interruption test.

## Boot environments

Boot environments use separate Linux roots and a dedicated systemd-boot EFI System Partition (ESP). Start with an inactive, single-filesystem base root containing the kernel. GNU `cp` clones the image and uses a reflink where supported. The destination must be a new directory.

```sh
cps boot-env activate --known-good --root base --esp esp \
    --root-partuuid "$base_partuuid" --initrd boot/initramfs.img
cps plan --root base --output upgrade.json update
cps boot-env stage --source base --root staged upgrade.json
# Deploy the staged directory to its own GPT root partition before activation.
cps boot-env activate --root staged --esp esp \
    --root-partuuid "$staged_partuuid" --initrd boot/initramfs.img
```

`stage` verifies the new root and gives it a fresh image identity. Activation copies boot files from the selected root and publishes its Boot Loader Specification entry last. Supply the **partition UUID** of each deployed root and the root-relative boot file paths. The kernel path defaults to `boot/vmlinuz`. Omit `--initrd` for a kernel that can mount the root on its own. Use `--options` for additional kernel arguments. Provisioning the partitions and installing systemd-boot on the dedicated ESP are external to these commands.

`--known-good` bootstraps the first entry. Later activations require the previous environment's known-good entry and boot files. A trial gets two attempts by default. Install [the `system/cps` package](packaging/cps.kdl) in the base image to make its health check part of `boot-complete.target`. The package ships the CPS binary, [the health unit](packaging/systemd/cps-boot-health.service), and the required dependency link. The unit runs `cps verify --root /` after `multi-user.target`. Systemd blesses the entry when boot completion succeeds, removing the boot counter. If the attempts run out, systemd-boot selects the previous environment. The managed ESP uses its own loader configuration and assumes no EFI default override. [systemd boot assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/)

### VM boot tests

The [VM test](tests/systemd-boot-vm.sh) builds a GPT disk with two separate ext4 roots and a FAT ESP, then boots it through OVMF and systemd-boot under QEMU. The `fallback` scenario checks that staging leaves the ESP alone and that the previous root boots after two failed attempts. The `bless` scenario runs systemd as PID 1, installs the health gate through the CPS package, and checks that systemd removes the counter and boots the blessed root again. The `health-fail` scenario corrupts a managed file and checks that the health gate blocks blessing through both attempts.

Build with `cargo build`. The test needs `qemu-system-x86_64`, `mkfs.fat`, mtools, `mkfs.ext4`, `sfdisk`, OpenSSL, systemd-boot, and matching non-Secure-Boot x86-64 OVMF code and variables files. Supply an EFI-stub Linux kernel with built-in virtio block, GPT, ext4, and serial console support, plus a compatible BusyBox with `sh`, `cat`, `reboot`, and `poweroff`.

```sh
CPS_VM_OVMF_CODE=/path/to/OVMF_CODE.fd \
CPS_VM_OVMF_VARS=/path/to/OVMF_VARS.fd \
CPS_VM_KERNEL=/path/to/vmlinuz \
CPS_VM_BUSYBOX=/path/to/busybox \
tests/systemd-boot-vm.sh bless
```

The script defaults to Arch Linux paths for the kernel, BusyBox, and systemd-boot. Set `CPS_VM_SYSTEMD_BOOT` if the EFI binary is elsewhere. It uses regular files and software emulation.

The blessing fixture also needs the local systemd userspace, util-linux, binutils `strip`, kmod, and kernel support for EFI variables and FAT. It copies the installed VFAT and ISO-8859-1 modules for the running kernel. Set `CPS_VM_KERNEL_RELEASE` for another installed kernel, or `CPS_VM_FAT_BUILTIN=1` if VFAT and its character sets are built in.
