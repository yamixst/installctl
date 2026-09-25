#!/bin/sh
set -u

# Resolve script path
SCRIPT_PATH="$0"
case "$SCRIPT_PATH" in
    /*) ;;
    */*) SCRIPT_PATH="$PWD/$SCRIPT_PATH" ;;
    *) SCRIPT_PATH="$(command -v "$SCRIPT_PATH" 2>/dev/null || printf '%s/%s' "$PWD" "$SCRIPT_PATH")" ;;
esac

# Create temporary directory
TMPDIR=$(mktemp -d "${TMPDIR:-/tmp}/installctl-uninstall.XXXXXX")
trap 'rm -rf "$TMPDIR"' EXIT INT TERM HUP

# Function to compute sha256 hex
hash_file() {
    _hf_file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$_hf_file" 2>/dev/null | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$_hf_file" 2>/dev/null | cut -d' ' -f1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$_hf_file" 2>/dev/null | awk '{print $NF}'
    fi
}

# Function to decode base64
decode_b64() {
    if command -v base64 >/dev/null 2>&1; then
        base64 -d 2>/dev/null || base64 -D 2>/dev/null
    elif command -v openssl >/dev/null 2>&1; then
        openssl base64 -d
    fi
}

# Decode blobs
awk -F'\t' -v dir="$TMPDIR" '
/^#@blob\t/ { cur = $2; file = dir "/b64_" cur; printf "" > (dir "/blob_" cur); next }
/^#@b64\t/ { print $2 >> file }
' "$SCRIPT_PATH"

for f in "$TMPDIR"/b64_*; do
    [ -f "$f" ] || continue
    idx=${f##*_}
    decode_b64 < "$f" > "$TMPDIR/blob_$idx"
done

# Read name
NAME=$(awk -F'\t' '/^#@name\t/ { print $2; exit }' "$SCRIPT_PATH")

# Extract and reverse items
sed -n 's/^#@item	//p' "$SCRIPT_PATH" | awk '{ a[NR] = $0 } END { for (i = NR; i >= 1; i--) print a[i] }' > "$TMPDIR/items.tsv"

needs_sudo() {
    [ "$(id -u)" = "0" ] && return 1
    _ns_target="$1"
    if [ -e "$_ns_target" ] || [ -L "$_ns_target" ]; then
        if [ ! -w "$_ns_target" ]; then
            return 0
        fi
        _ns_parent=$(dirname "$_ns_target")
        if [ ! -w "$_ns_parent" ]; then
            return 0
        fi
        return 1
    else
        _ns_p="$_ns_target"
        while [ -n "$_ns_p" ] && [ ! -e "$_ns_p" ] && [ ! -L "$_ns_p" ]; do
            _ns_parent=$(dirname "$_ns_p")
            [ "$_ns_parent" = "$_ns_p" ] && break
            _ns_p="$_ns_parent"
        done
        [ ! -w "$_ns_p" ]
    fi
}

run_as() {
    _ra_target="$1"
    shift
    if needs_sudo "$_ra_target"; then
        sudo "$@"
    else
        "$@"
    fi
}

printf 'This will undo the installation of %s:\n' "$NAME"

TAB="$(printf '\t')"
n=0

while IFS="$TAB" read -r kind f1 f2 f3 f4 rest; do
    [ -n "$kind" ] || continue
    n=$((n + 1))

    # Store raw line info
    printf '%s\t%s\t%s\t%s\t%s\n' "$kind" "$f1" "$f2" "$f3" "$f4" > "$TMPDIR/item_$n.info"

    runnable=1
    desc=""

    case "$kind" in
        created_file)
            path="$f1"
            sha256="$f2"
            if [ ! -e "$path" ] && [ ! -L "$path" ]; then
                desc="delete file $path — already gone"
            else
                cur_sha=$(hash_file "$path")
                if [ "$cur_sha" != "$sha256" ]; then
                    desc="delete file $path — CHANGED since install"
                else
                    desc="delete file $path"
                fi
            fi
            printf '[%d] %s\n' "$n" "$desc"
            ;;
        created_symlink)
            path="$f1"
            desc="delete symlink $path"
            printf '[%d] %s\n' "$n" "$desc"
            ;;
        created_dir)
            path="$f1"
            desc="remove directory $path (only if empty)"
            printf '[%d] %s\n' "$n" "$desc"
            ;;
        modified_patch)
            path="$f1"
            blob="$f2"
            tmp_patched="$TMPDIR/patch_tmp_$n"
            patch_ok=0
            if [ -f "$path" ]; then
                if patch -R -f -s -o "$tmp_patched" "$path" "$TMPDIR/blob_$blob" </dev/null >/dev/null 2>&1; then
                    patch_ok=1
                fi
            fi
            if [ "$patch_ok" -eq 1 ]; then
                touch "$TMPDIR/patch_ok_$n"
                desc="revert installer changes in $path"
                printf '[%d] %s\n' "$n" "$desc"
                diff -u "$path" "$tmp_patched" || true
            else
                rm -f "$tmp_patched"
                desc="revert installer changes in $path — your later edits conflict with the installer changes; cannot revert automatically"
                printf '[%d] %s\n' "$n" "$desc"
            fi
            ;;
        modified_restore)
            path="$f1"
            blob="$f2"
            desc="restore $path to its pre-install version"
            printf '[%d] %s\n' "$n" "$desc"
            if [ -f "$path" ] && [ -f "$TMPDIR/blob_$blob" ]; then
                d=$(diff -u "$path" "$TMPDIR/blob_$blob" 2>/dev/null || true)
                case "$d" in
                    *"Binary files"*) ;;
                    *) [ -n "$d" ] && printf '%s\n' "$d" ;;
                esac
            fi
            ;;
        deleted_file)
            path="$f1"
            desc="restore deleted file $path"
            printf '[%d] %s\n' "$n" "$desc"
            ;;
        deleted_symlink)
            path="$f1"
            desc="restore deleted symlink $path"
            printf '[%d] %s\n' "$n" "$desc"
            ;;
        deleted_dir)
            path="$f1"
            desc="restore deleted directory $path"
            printf '[%d] %s\n' "$n" "$desc"
            ;;
        action)
            root_user="$f1"
            cmd="$f2"
            inv="$f3"
            if [ "$inv" != "-" ] && [ -n "$inv" ]; then
                desc="run: $inv   (undo of: $cmd)"
                printf '[%d] %s\n' "$n" "$desc"
                runnable=1
            else
                desc="note: the installer ran \"$cmd\"; undo it by hand if needed"
                printf '[%d] %s\n' "$n" "$desc"
                runnable=0
            fi
            ;;
    esac

    printf '%s' "$desc" > "$TMPDIR/desc_$n"
    printf '%d' "$runnable" > "$TMPDIR/runnable_$n"
done < "$TMPDIR/items.tsv"

total=$n

printf 'Proceed? [a]ll / [s]elect / [c]ancel: '
read -r ans || ans="c"

all_chosen=1

case "$ans" in
    [aA]*)
        i=1
        while [ "$i" -le "$total" ]; do
            r=$(cat "$TMPDIR/runnable_$i")
            if [ "$r" -eq 1 ]; then
                touch "$TMPDIR/chosen_$i"
            fi
            i=$((i + 1))
        done
        ;;
    [sS]*)
        i=1
        while [ "$i" -le "$total" ]; do
            r=$(cat "$TMPDIR/runnable_$i")
            if [ "$r" -eq 1 ]; then
                item_desc=$(cat "$TMPDIR/desc_$i")
                printf '[%d] %s — do it? [y/N] ' "$i" "$item_desc"
                read -r item_ans || item_ans="n"
                case "$item_ans" in
                    [yY]*)
                        touch "$TMPDIR/chosen_$i"
                        ;;
                    *)
                        all_chosen=0
                        ;;
                esac
            fi
            i=$((i + 1))
        done
        ;;
    *)
        exit 1
        ;;
esac

has_failed=0
i=1

while [ "$i" -le "$total" ]; do
    if [ ! -f "$TMPDIR/chosen_$i" ]; then
        i=$((i + 1))
        continue
    fi

    IFS="$TAB" read -r kind f1 f2 f3 f4 rest < "$TMPDIR/item_$i.info"
    desc=$(cat "$TMPDIR/desc_$i")

    case "$kind" in
        created_file)
            path="$f1"
            if [ -e "$path" ] || [ -L "$path" ]; then
                run_as "$path" rm -f "$path" || true
            fi
            if [ -e "$path" ] || [ -L "$path" ]; then
                printf 'failed: [%d] %s\n' "$i" "$desc"
                has_failed=1
            fi
            ;;
        created_symlink)
            path="$f1"
            if [ -e "$path" ] || [ -L "$path" ]; then
                run_as "$path" rm -f "$path" || true
            fi
            if [ -e "$path" ] || [ -L "$path" ]; then
                printf 'failed: [%d] %s\n' "$i" "$desc"
                has_failed=1
            fi
            ;;
        created_dir)
            path="$f1"
            if [ -d "$path" ]; then
                run_as "$path" rmdir "$path" 2>/dev/null || true
            fi
            ;;
        modified_patch)
            path="$f1"
            mode="$f4"
            tmp_patched="$TMPDIR/patch_tmp_$i"
            if [ -f "$TMPDIR/patch_ok_$i" ] && [ -f "$tmp_patched" ]; then
                item_parent=$(dirname "$path")
                if [ ! -d "$item_parent" ]; then
                    run_as "$item_parent" mkdir -p "$item_parent" || true
                fi
                if run_as "$path" cp "$tmp_patched" "$path" && run_as "$path" chmod "$mode" "$path"; then
                    : ok
                else
                    printf 'failed: [%d] %s\n' "$i" "$desc"
                    has_failed=1
                fi
            else
                printf 'failed: [%d] revert installer changes in %s (conflicts with your edits)\n' "$i" "$path"
                has_failed=1
            fi
            ;;
        modified_restore)
            path="$f1"
            blob="$f2"
            mode="$f4"
            blob_file="$TMPDIR/blob_$blob"
            if [ -f "$blob_file" ]; then
                item_parent=$(dirname "$path")
                if [ ! -d "$item_parent" ]; then
                    run_as "$item_parent" mkdir -p "$item_parent" || true
                fi
                if run_as "$path" cp "$blob_file" "$path" && run_as "$path" chmod "$mode" "$path"; then
                    : ok
                else
                    printf 'failed: [%d] %s\n' "$i" "$desc"
                    has_failed=1
                fi
            else
                printf 'failed: [%d] %s\n' "$i" "$desc"
                has_failed=1
            fi
            ;;
        deleted_file)
            path="$f1"
            blob="$f2"
            mode="$f3"
            blob_file="$TMPDIR/blob_$blob"
            item_parent=$(dirname "$path")
            if [ ! -d "$item_parent" ]; then
                run_as "$item_parent" mkdir -p "$item_parent" || true
            fi
            if run_as "$path" cp "$blob_file" "$path" && run_as "$path" chmod "$mode" "$path"; then
                : ok
            else
                printf 'failed: [%d] %s\n' "$i" "$desc"
                has_failed=1
            fi
            ;;
        deleted_symlink)
            path="$f1"
            symlink_target="$f2"
            item_parent=$(dirname "$path")
            if [ ! -d "$item_parent" ]; then
                run_as "$item_parent" mkdir -p "$item_parent" || true
            fi
            if run_as "$path" ln -s "$symlink_target" "$path"; then
                : ok
            else
                printf 'failed: [%d] %s\n' "$i" "$desc"
                has_failed=1
            fi
            ;;
        deleted_dir)
            path="$f1"
            mode="$f2"
            if run_as "$path" mkdir -p "$path" && run_as "$path" chmod "$mode" "$path"; then
                : ok
            else
                printf 'failed: [%d] %s\n' "$i" "$desc"
                has_failed=1
            fi
            ;;
        action)
            root_user="$f1"
            inv="$f3"
            if [ "$inv" != "-" ] && [ -n "$inv" ]; then
                if [ "$root_user" = "root" ] && [ "$(id -u)" != "0" ]; then
                    if sudo sh -c "$inv"; then
                        : ok
                    else
                        printf 'failed: [%d] %s\n' "$i" "$desc"
                        has_failed=1
                    fi
                else
                    if sh -c "$inv"; then
                        : ok
                    else
                        printf 'failed: [%d] %s\n' "$i" "$desc"
                        has_failed=1
                    fi
                fi
            fi
            ;;
    esac

    i=$((i + 1))
done

if [ "$all_chosen" -eq 1 ] && [ "$has_failed" -eq 0 ]; then
    rm -f "$SCRIPT_PATH"
    printf 'Uninstall of %s complete.\n' "$NAME"
    exit 0
else
    printf 'Remove this uninstall script? [y/N] '
    read -r rm_ans || rm_ans="n"
    case "$rm_ans" in
        [yY]*) rm -f "$SCRIPT_PATH" ;;
    esac
    if [ "$has_failed" -ne 0 ]; then
        exit 2
    else
        exit 0
    fi
fi
