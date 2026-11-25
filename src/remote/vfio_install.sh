# The root-side body of the stackbox-vfio-bind installer. render_vfio_helper_install_script wraps
# it in a root bash heredoc and fills the @...@ placeholders; the helper is the last one filled.
# Every function either finishes its job or exits non-zero with a message on stderr.
set -eu
umask 022
agent_user='@AGENT_USER@'
helper_path='@HELPER_PATH@'
sudoers_path='@SUDOERS_PATH@'
helper_protocol='@HELPER_PROTOCOL@'
vfio_conf=/etc/stackbox/vfio.conf
vfio_group=kvm
group_note=

# write_helper: installs the helper as root:root 0755, atomically (temp file, then mv).
write_helper() {
    install -d -m 0755 /usr/local/libexec /etc/stackbox
    helper_tmp=/usr/local/libexec/.stackbox-vfio-bind.new
    cat > "$helper_tmp" <<'STACKBOX_HELPER_EOF'
@HELPER@
STACKBOX_HELPER_EOF
    if ! { chown root:root "$helper_tmp" && chmod 0755 "$helper_tmp"; }; then
        rm -f "$helper_tmp"
        echo 'stackbox: could not set the helper owner and mode; nothing was installed' >&2
        exit 1
    fi
    if ! mv -f "$helper_tmp" "$helper_path"; then
        rm -f "$helper_tmp"
        echo "stackbox: could not move the helper into $helper_path" >&2
        exit 1
    fi
}

# install_sudoers: writes the sudoers drop-in, checks it with visudo, then moves it into place.
# The rule names the helper alone, with no argument patterns: sudo-rs's visudo rejects wildcards in
# command arguments. The helper itself (root-owned, 0755) checks --check, bind and unbind and the BDF.
install_sudoers() {
    sudoers_tmp=/etc/sudoers.d/.stackbox-vfio.new
    cat > "$sudoers_tmp" <<STACKBOX_SUDOERS_EOF
Defaults!@HELPER_PATH@ env_reset
$agent_user ALL=(root) NOPASSWD: @HELPER_PATH@
STACKBOX_SUDOERS_EOF
    if ! { chown root:root "$sudoers_tmp" && chmod 0440 "$sudoers_tmp"; }; then
        rm -f "$sudoers_tmp"
        echo 'stackbox: could not set the sudoers owner and mode; nothing was installed for it' >&2
        exit 1
    fi
    if ! visudo -cf "$sudoers_tmp" >/dev/null; then
        rm -f "$sudoers_tmp"
        echo 'stackbox: the sudoers rule did not validate; nothing was installed for it' >&2
        exit 1
    fi
    if ! mv -f "$sudoers_tmp" "$sudoers_path"; then
        rm -f "$sudoers_tmp"
        echo "stackbox: could not move the sudoers rule into $sudoers_path" >&2
        exit 1
    fi
}

# verify_helper: runs the helper as the agent user through sudo -n, the way the coordinator will.
verify_helper() {
    if ! sudo -u "$agent_user" sudo -n "$helper_path" --check >/dev/null 2>&1; then
        echo "stackbox: $agent_user could not run the helper through sudo -n; check that the user exists and that $sudoers_path is in place" >&2
        exit 1
    fi
}

# check_vfio_group: the group the stackbox's VM opens /dev/vfio through (GROUP= in the config,
# else kvm). Records whether the agent user is already in it; otherwise records the usermod step.
check_vfio_group() {
    if [ -r "$vfio_conf" ]; then
        configured=$(sed -n 's/^GROUP=//p' "$vfio_conf" || true)
        if [ -n "$configured" ]; then vfio_group=$configured; fi
    fi
    case "$vfio_group" in
        '' | [!a-z_]* | *[!a-z0-9_-]*)
            echo "stackbox: GROUP in $vfio_conf must match [a-z_][a-z0-9_-]*" >&2
            exit 1
            ;;
    esac
    case " $(id -nG "$agent_user") " in
        *" $vfio_group "*) group_note='이미 포함됨' ;;
        *) group_note="미포함 → sudo usermod -aG $vfio_group $agent_user 실행 후 재로그인 필요" ;;
    esac
}

# print_success: the summary the installer ends with.
print_success() {
    echo '✓ stackbox-vfio-bind 설치 완료!'
    echo "  - 헬퍼: $helper_path (protocol=$helper_protocol)"
    echo "  - sudoers: $sudoers_path (visudo 검증 통과, $agent_user sudo -n 확인)"
    echo "  - VFIO 그룹($vfio_group): $group_note"
    echo "  → 이제 Stackbox 포탈(/hosts)에서 'SSH 점검'을 누르면 원클릭 바인딩을 사용할 수 있습니다."
}

main() {
    write_helper
    install_sudoers
    verify_helper
    check_vfio_group
    print_success
}

main
