#!/bin/sh
set -e

mkdir -p "$HOME/.local/share/fakeapp"
cat << 'EOF' > "$HOME/.local/share/fakeapp/fakeapp"
#!/bin/sh
echo fakeapp
EOF
chmod 755 "$HOME/.local/share/fakeapp/fakeapp"

mkdir -p "$HOME/.local/bin"
ln -sf "$HOME/.local/share/fakeapp/fakeapp" "$HOME/.local/bin/fakeapp"

echo 'export PATH="$HOME/.local/bin:$PATH"' >> "$HOME/.bashrc"

sh -c 'sh -c "echo grandchild > \"$HOME/.local/share/fakeapp/gc.txt\""'

mkdir -p "$HOME/.fakeapp-tmp"
rm -rf "$HOME/.fakeapp-tmp"

sudo sh -c 'cat << "EOF" > /usr/local/bin/fakeapp-helper
#!/bin/sh
echo fakeapp-helper
EOF
chmod 755 /usr/local/bin/fakeapp-helper'

sudo apt-get install -y fakepkg

systemctl --user enable --now fakeapp.service

(sleep 300 &)
sleep 0.05

echo done
