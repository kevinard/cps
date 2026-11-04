#!/usr/bin/env bash
# Capture the C compiler and build utilities on an Arch Linux test worker.
set -euo pipefail
unset LD_LIBRARY_PATH LD_PRELOAD GCC_EXEC_PREFIX COMPILER_PATH LIBRARY_PATH CPATH C_INCLUDE_PATH
export PATH=/usr/bin:/bin LC_ALL=C
umask 022

usage='usage: seed-native-c.sh PROTOTYPE [--rust|--kernel|--systemd|--util-linux|--busybox]'
seed=${1:?$usage}
rust_root=
kernel=false
systemd=false
util_linux=false
busybox=false
if [[ $# == 2 && $2 == --rust ]]; then
    rust_root=$(rustc --print sysroot)
elif [[ $# == 2 && $2 == --kernel ]]; then
    kernel=true
elif [[ $# == 2 && $2 == --systemd ]]; then
    systemd=true
elif [[ $# == 2 && $2 == --util-linux ]]; then
    util_linux=true
elif [[ $# == 2 && $2 == --busybox ]]; then
    busybox=true
elif [[ $# != 1 ]]; then
    echo "$usage" >&2
    exit 2
fi
mkdir -- "$seed"

copy_file() {
    local source=$1 path=$2
    mkdir -p -- "$seed/${path%/*}"
    cp -L -- "$source" "$seed/$path"
}

copy_elf() {
    local source=$1 path=$2 interpreter loader dependencies library relative
    copy_file "$source" "$path"
    interpreter=$(readelf -l "$source" | sed -n 's/^.*Requesting program interpreter: \([^]]*\)].*$/\1/p')
    if [[ -n $interpreter ]]; then
        copy_file "$interpreter" "${interpreter#/}"
        loader=$(realpath -- "$interpreter")
        copy_file "$loader" "${loader#/}"
        dependencies=$(ldd "$source")
        if [[ $dependencies == *'not found'* ]]; then
            printf 'Missing shared library for %s\n%s\n' "$source" "$dependencies" >&2
            return 1
        fi
        while IFS= read -r library; do
            library=$(realpath -ms -- "$library")
            relative=${library#/}
            if [[ -n $rust_root && $library == "$rust_root/"* ]]; then
                relative="usr/${library#"$rust_root/"}"
            fi
            copy_file "$library" "$relative"
        done < <(awk '$2 == "=>" && $3 ~ /^\// { print $3 }' <<< "$dependencies")
    fi
}

copy_elf /usr/bin/bash bin/sh
ln -s sh "$seed/bin/bash"
for tool in ar as awk basename bzip2 cat chmod cmp cp cut date dd diff dirname echo expr find \
    gcc grep head install ld ln make mkdir mktemp mv nm objcopy objdump od rm sed sleep sort strip \
    tail tar touch tr uname uniq wc whoami xargs yes; do
    copy_elf "/usr/bin/$tool" "usr/bin/$tool"
done
for tool in cc1 collect2 lto-wrapper; do
    binary=$(gcc -print-prog-name="$tool")
    copy_elf "$binary" "${binary#/}"
done
for file in crt1.o Scrt1.o crti.o crtn.o crtbegin.o crtbeginS.o crtbeginT.o crtend.o \
    crtendS.o liblto_plugin.so libgcc.a libgcc_eh.a libgcc_s.so libgcc_s.so.1 libc.so \
    libm.so libmvec.so.1; do
    path=$(realpath -ms -- "$(gcc -print-file-name="$file")")
    copy_file "$path" "${path#/}"
done
for file in libgcc_s_asneeded.so libatomic_asneeded.a libatomic_asneeded.so \
    libatomic.a libatomic.so libatomic.so.1; do
    path=$(gcc -print-file-name="$file")
    if [[ $path == /* ]]; then
        path=$(realpath -ms -- "$path")
        copy_file "$path" "${path#/}"
    fi
done
gcc_include=$(gcc -print-file-name=include)
mkdir -p -- "$seed/${gcc_include#/}"
cp -RL -- "$gcc_include/." "$seed/${gcc_include#/}/"
libc_files=$(pacman -Qlq glibc linux-api-headers)
while IFS= read -r file; do
    copy_file "$file" "${file#/}"
done < <(awk '/^\/usr\/include\/.*[^\/]$/ || /^\/usr\/lib\/[^/]+\.a$/' <<< "$libc_files")

if $busybox; then
    copy_elf /usr/bin/patch usr/bin/patch
fi
if $util_linux; then
    for tool in env ls pkg-config printf readlink true xz; do
        copy_elf "/usr/bin/$tool" "usr/bin/$tool"
    done
    ln -s gcc "$seed/usr/bin/cc"
fi
if [[ -n $rust_root ]]; then
    rust_host=$(rustc -vV | sed -n 's/^host: //p')
    test -n "$rust_host"
    for tool in rustc cargo; do
        copy_elf "$rust_root/bin/$tool" "usr/bin/$tool"
    done
    mkdir -p "$seed/usr/lib/rustlib"
    cp -RL -- "$rust_root/lib/rustlib/$rust_host" "$seed/usr/lib/rustlib/"
    ln -s gcc "$seed/usr/bin/cc"
    for tool in gzip ls pkg-config ranlib rmdir; do
        copy_elf "/usr/bin/$tool" "usr/bin/$tool"
    done
fi
if $kernel; then
    for tool in bison flex gzip ls m4 pkg-config readelf realpath sha1sum true xz; do
        copy_elf "/usr/bin/$tool" "usr/bin/$tool"
    done
    copy_elf "${CPS_KERNEL_BC:-/usr/bin/bc}" usr/bin/bc
    # x86's objtool links against libelf, also used by the seeded binutils.
    for path in usr/include/{gelf,libelf}.h usr/lib/{libelf.so.1,pkgconfig/libelf.pc}; do
        copy_file "/$path" "$path"
    done
    ln -s libelf.so.1 "$seed/usr/lib/libelf.so"
    mkdir -p -- "$seed/usr/share"
    cp -RL -- /usr/share/bison "$seed/usr/share/"
fi
if $systemd; then
    tools_root=${CPS_SYSTEMD_TOOLS_ROOT:-/}
    tools_root=${tools_root%/}
    for tool in env gzip id ls ninja pkg-config printf python python3 readelf realpath stat true; do
        copy_elf "/usr/bin/$tool" "usr/bin/$tool"
    done
    ln -s gcc "$seed/usr/bin/cc"
    copy_file /usr/bin/meson usr/bin/meson
    for tool in gperf patchelf; do
        copy_elf "$tools_root/usr/bin/$tool" "usr/bin/$tool"
    done
    python_lib=$(python3 -c 'import sysconfig; print(sysconfig.get_path("stdlib"))')
    python_files=$(pacman -Qlq python)
    while IFS= read -r path; do
        copy_file "$path" "${path#/}"
    done < <(awk -v dir="$python_lib/" 'index($0, dir) == 1 && !/\/$|__pycache__|\/site-packages\//' <<< "$python_files")
    site="$python_lib/site-packages"
    mkdir -p "$seed$site"
    for module in mesonbuild packaging markupsafe; do
        cp -RL -- "$site/$module" "$seed$site/"
    done
    for module in elftools jinja2; do
        cp -RL -- "$tools_root$site/$module" "$seed$site/"
    done
    for path in usr/include/{seccomp.h,seccomp-syscalls.h} usr/lib/pkgconfig/{libseccomp,libsystemd}.pc; do
        copy_file "/$path" "$path"
    done
    util_root=${CPS_SYSTEMD_UTIL_LINUX_ROOT:-}
    if [[ -z $util_root ]]; then
        copy_elf /usr/bin/mount usr/bin/mount
    else
        copy_file "$util_root/usr/bin/mount" usr/bin/mount
    fi
    for path in usr/include/{blkid/blkid.h,libmount/libmount.h} \
        usr/lib/pkgconfig/{blkid,mount}.pc usr/lib/{libblkid.so.1,libmount.so.1}; do
        copy_file "$util_root/$path" "$path"
    done
    ln -s libblkid.so.1 "$seed/usr/lib/libblkid.so"
    ln -s libmount.so.1 "$seed/usr/lib/libmount.so"
fi
