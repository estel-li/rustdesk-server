#!/bin/sh
# CE: 容器启动时自愈 data / sock 目录所有权,然后降权到 rustdesk (UID 1000)。
#
# 解决的问题:Linux 上 bind mount 进来的 host 目录原样保留 host 端 owner,
# 如果 host 端目录不是 1000:1000 拥有,容器内 UID 1000 的 hbbs/hbbr 就写不进
# (典型症状: "unable to open database file" / "Failed to create directory")。
# macOS docker desktop 有透明 UID remap,这里的 chown 等价 no-op。
#
# 必须以 root 启动才能 chown 任意 host owner 的目录。chown 完后用 setpriv
# 降权,实际进程仍以 rustdesk:rustdesk 跑(安全姿势不变)。
set -e

# 只 chown 已经存在的数据 / sock 目录,不存在的跳过。
# /run/secrets 是 ro mount,千万不能 chown,排除在外。
for d in /var/lib/rustdesk-server /run/rustdesk-server; do
    if [ -d "$d" ]; then
        chown -R rustdesk:rustdesk "$d" 2>/dev/null || \
            echo "warning: chown $d failed (read-only? ignore if intentional)" >&2
    fi
done

# setpriv 在 util-linux 里,bookworm-slim 默认带。--init-groups 会读 /etc/group
# 把 rustdesk 的辅助组也加上(本镜像里只有 rustdesk 主组,等价 -g 1000)。
exec setpriv --reuid=1000 --regid=1000 --init-groups -- "$@"
