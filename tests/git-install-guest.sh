#!/bin/sh
set -eu
bb=/bin/busybox
export PATH=/usr/bin:/bin
export SUDO_UID=1000 SUDO_GID=1000
trap 'echo "CPS_VM_GIT: failed"; $bb poweroff -f' EXIT
work=/volume/@captures
tracked=$work/tracked
pinned=$work/pinned

check_package() {
    test "$($bb cat "/usr/share/git-$1")" = "$2"
    cps info "test/git-$1" | $bb grep -E "upstream-version=\"?$3\"?( |$)"
}

if [ -f "$work/git-phase" ]; then
    old=$($bb cat "$work/git-old")
    new=$($bb cat "$work/git-new")
    fixed=$($bb cat "$work/git-fixed")
    check_package pinned 'pinned initial' "$fixed"
    cps verify --root /
    if [ "$($bb cat "$work/git-phase")" = trial ]; then
        check_package tracked 'tracked updated' "$new"
        systemctl is-active --quiet systemd-bless-boot.service
        echo 'CPS_VM_GIT: tracked package advanced and pinned package stayed at its commit'
        cps boot rollback --esp /efi --btrfs-volume /volume
        echo base > "$work/git-phase"
        $bb sync
        exec $bb reboot -f
    fi
    check_package tracked 'tracked initial' "$old"
    cps upgrade --no-refresh --dry-run | $bb grep -Fx 'Nothing to do.'
    rewritten=$(git -C "$tracked" -c user.name='CPS Test' -c user.email=test@example.invalid \
        commit-tree 'HEAD^{tree}' -m rewrite)
    git -C "$tracked" update-ref refs/heads/main "$rewritten"
    cps upgrade --yes > /run/git-rewrite 2>&1
    $bb grep -F 'branch does not descend from the installed Git commit' /run/git-rewrite
    check_package tracked 'tracked initial' "$old"
    echo 'CPS_VM_GIT: rollback restored the tracked commit and payload'
    trap - EXIT
    exec $bb poweroff -f
fi

$bb cp /repository/catalog-1.0.0-0 /repository/current
! $bb grep -Fq git-toolchain-config /etc/cps/image.kdl
write_repo() {
    directory=$1
    package=$2
    toolchain=$3
    $bb mkdir "$directory"
    git -C "$directory" init --quiet --initial-branch=main --template=
    $bb cat > "$directory/cps.kdl" <<KDL
schema-version 1 kind="git-package"
package "test/git-$package" version="1.0.0-0" {
    file "usr/share/git-$package" mode="0644" uid=0 gid=0
}
build architecture="x86_64" epoch=1700000000 {
    $toolchain
    script """
    tar -xf "\$CPS_SOURCES/git.tar"
    install -Dm644 payload "\$DESTDIR/usr/share/git-$package"
    """
}
KDL
    echo "$package initial" > "$directory/payload"
    git -C "$directory" add .
    git -C "$directory" -c user.name='CPS Test' -c user.email=test@example.invalid commit -qm initial
}
write_repo "$tracked" tracked 'toolchain "default"'
write_repo "$pinned" pinned "$($bb sed '1d' /usr/share/cps/git-build/toolchain.default.kdl)"
old=$(git -C "$tracked" rev-parse HEAD)
fixed=$(git -C "$pinned" rev-parse HEAD)
echo "$old" > "$work/git-old"
echo "$fixed" > "$work/git-fixed"
cps install --yes --git "$tracked" --track-branch main
cps install --yes --git "$pinned" --commit "$fixed"
check_package tracked 'tracked initial' "$old"
check_package pinned 'pinned initial' "$fixed"
for package in tracked pinned; do
    directory=$work/$package
    echo "$package updated" > "$directory/payload"
    git -C "$directory" -c user.name='CPS Test' -c user.email=test@example.invalid commit -qam advance
done
git -C "$tracked" rev-parse HEAD > "$work/git-new"
cps upgrade --dry-run | $bb grep -F "$($bb cat "$work/git-new")"
check_package tracked 'tracked initial' "$old"
cps upgrade --yes
check_package tracked 'tracked initial' "$old"
echo trial > "$work/git-phase"
$bb sync
exec $bb reboot -f
