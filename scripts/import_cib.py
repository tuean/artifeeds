#!/usr/bin/env python3
"""Import timqian/chinese-independent-blogs into Artifeeds config files.

    python3 scripts/import_cib.py [--out config] [--csv /tmp/cib.csv]

Reads the upstream CSV (downloading it if not present), then writes

    <out>/feeds.toml            settings tuned for ~1 400 feeds + `include`
    <out>/blogs/<category>.toml one file per coarse category

Every blog becomes one `[[sources]]` entry with

    title   = the blog's own name          <- the "blog dimension"
    group   = coarse category              <- the grouped dimension
    tags    = the upstream tag list
    key     = the feed URL (stable identity across URL edits)
    url     = the feed URL

Splitting by category keeps each file reviewable and lets `include` pick up new
ones automatically, while the app can still present the flat per-blog list.
"""

from __future__ import annotations

import argparse
import collections
import csv
import os
import re
import sys
import unicodedata
import urllib.request

UPSTREAM = "https://raw.githubusercontent.com/timqian/chinese-independent-blogs/master/blogs-original.csv"
DEFAULT_CSV = "/tmp/cib.csv"

# Coarse buckets, in match order. The first tag of a blog (its primary tag) is
# matched against these patterns; anything unmatched lands in 其他.
CATEGORIES: list[tuple[str, tuple[str, ...]]] = [
    ("技术", ("编程", "技术", "前端", "后端", "算法", "linux", "unix", "windows", "mac",
              "ai", "机器学习", "深度学习", "安全", "运维", "数据库", "开源", "架构",
              "教程", "笔记", "工具", "代码", "java", "python", "rust", "go", "c++",
              "javascript", "typescript", "网络", "性能", "测试", "devops", "云")),
    ("生活", ("生活", "日常", "记录", "随笔", "随想", "折腾", "摄影", "美食", "旅行",
              "家庭", "育儿", "健康", "运动", "游戏", "音乐", "电影", "设计", "手账")),
    ("阅读", ("读书", "阅读", "书", "思考", "分享", "学习", "成长", "知识", "科普")),
    ("职场", ("创业", "产品", "职场", "商业", "金融", "投资", "管理", "经济")),
]


def fetch(url: str, dest: str) -> str:
    if not os.path.exists(dest):
        print(f"downloading {url}", file=sys.stderr)
        with urllib.request.urlopen(url, timeout=60) as r, open(dest, "wb") as f:
            f.write(r.read())
    return dest


def category_of(tags: list[str]) -> str:
    haystack = " ".join(t.lower() for t in tags)
    for name, keys in CATEGORIES:
        for key in keys:
            if key in haystack:
                return name
    return "其他"


def slug(name: str) -> str:
    """ASCII-ish file name for a category."""
    table = {"技术": "tech", "生活": "life", "阅读": "reading", "职场": "career", "其他": "misc"}
    return table.get(name, "misc")


def split_tags(raw: str) -> list[str]:
    parts = [t.strip() for t in re.split(r"[;；,，|]", raw)]
    seen: list[str] = []
    for part in parts:
        if part and part not in seen:
            seen.append(part)
    return seen


def toml_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def normalize_url(url: str) -> str:
    """Mirror artifeeds-core's normalisation so duplicates are caught here."""
    url = url.strip()
    if not url:
        return ""
    if not url.startswith(("http://", "https://")):
        url = "https://" + url
    scheme, _, rest = url.partition("://")
    authority, slash, path = rest.partition("/")
    path = "/" + path if slash else "/"
    while path.endswith("/") and len(path) > 1:
        path = path[:-1]
    return f"{scheme.lower()}://{authority.lower()}{path}"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="config", help="output directory")
    ap.add_argument("--csv", default=DEFAULT_CSV, help="cached CSV path")
    ap.add_argument("--limit", type=int, default=0, help="debug: only import N blogs")
    args = ap.parse_args()

    path = fetch(UPSTREAM, args.csv)
    with open(path, newline="", encoding="utf-8") as f:
        rows = [{k.strip(): (v or "").strip() for k, v in r.items()} for r in csv.DictReader(f)]

    buckets: dict[str, list[dict]] = collections.defaultdict(list)
    seen_feeds: set[str] = set()
    skipped_no_feed = skipped_dup = skipped_bad = 0

    for row in rows:
        name = row.get("Introduction", "").strip()
        feed = normalize_url(row.get("RSS feed", ""))
        site = row.get("Address", "").strip()
        tags = split_tags(row.get("tags", ""))

        if not name or not feed:
            skipped_no_feed += 1
            continue
        if not feed.startswith(("http://", "https://")) or "." not in feed:
            skipped_bad += 1
            continue
        if feed in seen_feeds:
            skipped_dup += 1
            continue
        seen_feeds.add(feed)

        category = category_of(tags)
        buckets[category].append(
            {"name": name, "feed": feed, "site": site, "tags": tags, "category": category}
        )
        if args.limit and sum(len(v) for v in buckets.values()) >= args.limit:
            break

    os.makedirs(os.path.join(args.out, "blogs"), exist_ok=True)
    total = 0
    for category, entries in sorted(buckets.items(), key=lambda kv: -len(kv[1])):
        entries.sort(key=lambda e: unicodedata.normalize("NFKC", e["name"]).lower())
        lines = [
            f"# 中文独立博客 — {category} ({len(entries)} feeds)",
            "# Generated by scripts/import_cib.py from timqian/chinese-independent-blogs.",
            "# Edit by hand is fine; re-running the importer overwrites this file.",
            "",
        ]
        for e in entries:
            lines.append("[[sources]]")
            lines.append(f'title = "{toml_escape(e["name"])}"')
            lines.append(f'url   = "{e["feed"]}"')
            lines.append(f'group = "{category}"')
            if e["tags"]:
                rendered = ", ".join(f'"{toml_escape(t)}"' for t in e["tags"])
                lines.append(f"tags  = [{rendered}]")
            lines.append(f'key   = "{e["feed"]}"')
            lines.append("")
        dest = os.path.join(args.out, "blogs", f"{slug(category)}.toml")
        with open(dest, "w", encoding="utf-8") as f:
            f.write("\n".join(lines))
        total += len(entries)
        print(f"{dest:40} {len(entries):>5} feeds")

    feeds_toml = ROOT_TEMPLATE
    with open(os.path.join(args.out, "feeds.toml"), "w", encoding="utf-8") as f:
        f.write(feeds_toml)

    print()
    print(f"imported        {total} feeds")
    print(f"skipped (no rss){skipped_no_feed}")
    print(f"skipped (dup)   {skipped_dup}")
    print(f"skipped (bad)   {skipped_bad}")
    print(f"wrote           {os.path.join(args.out, 'feeds.toml')}")
    return 0


ROOT_TEMPLATE = """# Artifeeds — 中文独立博客源
#
# 由 scripts/import_cib.py 生成，数据来自
# https://github.com/timqian/chinese-independent-blogs
#
# `include` 必须放在任何 [table] 之前（TOML 规则）：其后出现的键会归属该 table。
include = ["blogs"]

[settings]
# 约 1 400 个独立博客：放宽并发，但对单个域名保持克制。
# 独立博客分布在上千个不同域名上：per-host 限制几乎不会成为瓶颈，
# 全局并发才是决定首次刷新耗时的因素。对单个域名依旧保持克制。
global_concurrency     = 24
per_host_concurrency   = 2
host_min_gap_ms        = 400
max_concurrent_fetches = 24

# 大量站点不可达/极慢：尽早失败，把带宽让给活着的源。
connect_timeout_secs = 5
request_timeout_secs = 30

# 独立博客更新频率低：基础间隔 1 小时，安静时最多拉长到 4 小时。
default_interval_secs = 3600
min_interval_secs     = 900
max_interval_secs     = 86400
quiet_factor          = 4
jitter_percent        = 12

# 首次启动把 1 400 个源分摊到 45 秒内，避免瞬时打满连接数。
startup_spread_secs   = 45

# 归档型 feed 只取最新 60 条，首次全量刷新才不会失控。
max_entries_per_fetch  = 60
# 归档型 feed 能到十几 MB（全量 content:encoded）；太小会把它们判为失败。
max_body_bytes         = 16777216
preview_chars          = 400
max_content_bytes      = 65536

# 窗口大小 = 内存开销上限；SQLite 里保存全部历史，搜索覆盖全库。
max_in_memory_articles = 20000
retention_days         = 90
max_articles           = 150000
full_text_search       = true

# 阅读模式首次打开时把原站整页存成本地自包含 HTML（内含 CSS/图片/字体），
# 之后离线渲染；独立博客常见改版/删站，存档也顺手解决了链接失效。
archive_on_open            = true
archive_max_bytes          = 12582912
archive_max_resource_bytes = 2097152
archive_max_resources      = 80

user_agent = "Artifeeds/0.1 (feed reader; +https://github.com/local/artifeeds)"
accept_language = "zh-CN,zh;q=0.9,en;q=0.8"

[group_defaults."技术"]
interval_secs = 3600

[group_defaults."生活"]
interval_secs = 7200

[group_defaults."阅读"]
interval_secs = 7200

[group_defaults."职场"]
interval_secs = 5400

[group_defaults."其他"]
interval_secs = 7200
"""

if __name__ == "__main__":
    raise SystemExit(main())
