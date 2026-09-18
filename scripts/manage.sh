#!/bin/sh
set -eu
: "${HERDR_PLUGIN_ROOT:?Herdr must provide the plugin root}"
revive_bin="$HERDR_PLUGIN_ROOT/target/release/herdr-revive"

pick_space() {
    names=$("$revive_bin" space names) || return 1
    if [ -z "$names" ]; then
        printf 'No saved spaces.\n' >&2
        return 1
    fi
    if command -v fzf >/dev/null 2>&1 && [ -t 0 ]; then
        name=$(printf '%s\n' "$names" | fzf --prompt='Saved space> ') || return 1
    else
        printf '%s\nSpace name: ' "$names"
        IFS= read -r name || return 1
    fi
}

space_action() {
    case "$1" in
        save-space)
            printf 'Save this workspace as: '
            IFS= read -r name || return 1
            "$revive_bin" space save "$name" ;;
        open-space)
            pick_space || return 1
            "$revive_bin" space preview "$name" || return 1
            printf 'Open as a new workspace? Type open: '
            IFS= read -r answer || return 1
            [ "$answer" != open ] || "$revive_bin" space open "$name" ;;
        delete-space)
            pick_space || return 1
            printf 'Delete "%s"? Type delete: ' "$name"
            IFS= read -r answer || return 1
            [ "$answer" != delete ] || "$revive_bin" space delete "$name" ;;
    esac
}

if [ "$#" -gt 0 ]; then
    space_action "$1"
    exit
fi
while :; do
    printf '\nherdr-revive\n1 Save session\n2 Preview session restore\n3 Restore session\n4 List snapshots\n5 Save named space\n6 Open named space\n7 Delete named space\n8 List named spaces\n9 Restore selected snapshot\n0 Exit\n> '
    IFS= read -r choice || exit 0
    case "$choice" in
        0) exit 0 ;;
        1) "$revive_bin" save || printf 'Save failed.\n' >&2 ;;
        2) "$revive_bin" preview || printf 'Preview failed.\n' >&2 ;;
        3|9)
            if [ "$choice" = 9 ]; then
                "$revive_bin" list || continue
                printf 'Snapshot path: '
                IFS= read -r snapshot || exit 0
                set -- --snapshot "$snapshot"
            else
                set --
            fi
            "$revive_bin" preview "$@" || continue
            printf 'Restore matching idle panes and missing workspaces? Type restore: '
            IFS= read -r answer || exit 0
            [ "$answer" != restore ] || "$revive_bin" restore "$@" || printf 'Restore failed.\n' >&2 ;;
        4) "$revive_bin" list || printf 'List failed.\n' >&2 ;;
        5) space_action save-space || printf 'Save cancelled or failed.\n' >&2 ;;
        6) space_action open-space || printf 'Open cancelled or failed.\n' >&2 ;;
        7) space_action delete-space || printf 'Delete cancelled or failed.\n' >&2 ;;
        8) "$revive_bin" space list || printf 'List failed.\n' >&2 ;;
        *) printf 'Choose a listed number.\n' ;;
    esac
done
