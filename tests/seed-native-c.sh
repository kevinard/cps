#!/usr/bin/env bash
# Capture the C compiler and build utilities on an Arch Linux test worker.
set -euo pipefail
unset LD_LIBRARY_PATH LD_PRELOAD GCC_EXEC_PREFIX COMPILER_PATH LIBRARY_PATH CPATH C_INCLUDE_PATH
export PATH=/usr/bin:/bin LC_ALL=C
umask 022

seed=${1:?usage: seed-native-c.sh PROTOTYPE}
mkdir -- "$seed"

copy_file() {
    local source=$1 path=$2
    mkdir -p -- "$seed/${path%/*}"
    cp -L -- "$source" "$seed/$path"
}

copy_elf() {
    local source=$1 path=$2 interpreter loader dependencies library
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
            copy_file "$library" "${library#/}"
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
