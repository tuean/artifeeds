//! Finding the same blog subscribed twice.
//!
//! A feed reaches a reader under several addresses — `https://blog.example/feed`,
//! `http://blog.example/index.xml`, a FeedBurner or feedx proxy in front of
//! either — and every list that gets imported brings its own spelling. The loader
//! deduplicates by URL, which is the right rule for *identity* and the wrong one
//! for *meaning*: two addresses of one blog are two subscriptions, two sets of
//! unread counts, and two rows in the sidebar.
//!
//! What is knowable without fetching anything is the feed's own **title**, which
//! is the same whichever address it is served from. That is the signal here, with
//! the host as a tiebreaker, and the choice of survivor is a rule rather than a
//! guess: prefer the blog's own address over a proxy's, prefer `https`, prefer
//! the shorter URL, and when nothing else separates them, whatever sorts first —
//! so running the tool twice proposes the same thing.

/// One blog subscribed more than once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// The title the feeds share, as it appeared (before normalising).
    pub title: String,
    /// The subscription to keep.
    pub keep: String,
    /// The ones to remove, with the reason each was judged a duplicate.
    pub drop: Vec<(String, String)>,
}

/// Hosts that serve somebody else's feed. A subscription on one of these is the
/// same blog as the one behind it, and is the worse address to keep: the proxy
/// can disappear, and it usually rewrites the links.
fn is_proxy_host(host: &str) -> bool {
    const PROXIES: [&str; 8] = [
        "feedburner.com",
        "feedx.net",
        "feedsky.com",
        "rsshub.app",
        "feeds.feedblitz.com",
        "feedproxy.google.com",
        "rsshub.yducs.xyz",
        "rsshub.cc",
    ];
    let bare = host.strip_prefix("www.").unwrap_or(host);
    PROXIES.iter().any(|p| bare == *p || bare.ends_with(&format!(".{p}")))
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .map(|host| host.trim_start_matches("www.").to_lowercase())
        .unwrap_or_default()
}

/// A title only identifies a blog when it says something. `Home`, `博客` and
/// `RSS` are shared by thousands of feeds that have nothing to do with each other.
fn distinctive(title: &str) -> bool {
    const GENERIC: [&str; 14] = [
        "home", "blog", "rss", "feed", "atom", "index", "posts", "articles", "首页", "博客",
        "订阅", "文章", "最新", "全部",
    ];
    let trimmed = title.trim();
    if trimmed.chars().count() < 4 {
        return false;
    }
    let lowered = trimmed.to_lowercase();
    !GENERIC.iter().any(|g| lowered == *g)
}

/// How the title is compared: case, spacing and punctuation are not meaning.
pub fn normalise_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| !c.is_whitespace() && !"|·-—–_[]（）()".contains(*c))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Which of two addresses of the same blog to keep. Lower is better.
fn rank(url: &str) -> (u8, u8, usize, String) {
    let host = host_of(url);
    let proxy = u8::from(is_proxy_host(&host));
    let insecure = u8::from(!url.starts_with("https://"));
    (proxy, insecure, url.len(), url.to_string())
}

/// The same blog under more than one address.
///
/// `min_title` guards against grouping by a title too generic to be an identity;
/// callers pass the same value they would pass to [`distinctive`] if they wanted
/// a different threshold.
pub fn find_duplicates<'a>(
    sources: impl IntoIterator<Item = (Option<&'a str>, &'a str)>,
) -> Vec<DuplicateGroup> {
    // Title → the addresses claiming it.
    let mut by_title: Vec<(String, Vec<(String, &'a str)>)> = Vec::new();
    for (title, url) in sources {
        let Some(title) = title else { continue };
        if !distinctive(title) {
            continue;
        }
        let key = normalise_title(title);
        let entry = (title.to_string(), url);
        match by_title.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.push(entry),
            None => by_title.push((key, vec![entry])),
        }
    }

    let mut out = Vec::new();
    for (_, group) in by_title {
        if group.len() < 2 {
            continue;
        }
        let title = group[0].0.clone();
        let mut urls: Vec<&str> = group.iter().map(|(_, url)| *url).collect();
        urls.sort_by_key(|url| rank(url));
        urls.dedup();
        if urls.len() < 2 {
            continue;
        }
        let keep = urls[0].to_string();
        let host = host_of(&keep);
        let drop = urls[1..]
            .iter()
            .map(|url| {
                let other = host_of(url);
                let reason = if is_proxy_host(&other) {
                    format!("{other} 是转发服务，保留博客自己的地址")
                } else if other == host {
                    format!("同一站点的另一个 feed 地址（{other}）")
                } else {
                    format!("标题与《{title}》完全相同（{other}）")
                };
                ((*url).to_string(), reason)
            })
            .collect();
        out.push(DuplicateGroup { title, keep, drop });
    }
    out.sort_by(|a, b| a.title.cmp(&b.title));
    out
}

/// Remove several subscriptions from one file in a single write.
///
/// One write per file, not per feed: the backup is the file as it was, and
/// nothing is rewritten twice.
pub fn remove_sources_from_file(file: &str, urls: &[String]) -> Result<usize, String> {
    let path = std::path::Path::new(file);
    if !path.is_file() {
        return Err(format!("{file} 不是一个文件，无法编辑"));
    }
    let original = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    // The loader reads TOML lists and OPML exports, so removal has to understand
    // both: a feed imported from an OPML lives in that OPML, and a TOML-shaped
    // edit would report "not found" for a subscription that is plainly there.
    let is_opml = file.to_lowercase().ends_with(".opml") || file.to_lowercase().ends_with(".xml");
    let mut updated = original.clone();
    let mut removed = 0usize;
    for url in urls {
        let next = if is_opml {
            crate::config::remove_source_from_opml(&updated, url)
        } else {
            crate::config::remove_source_from_toml(&updated, url)
        };
        if let Some(next) = next {
            updated = next;
            removed += 1;
        }
    }
    if removed == 0 {
        return Err(format!("在 {file} 里找不到这些源"));
    }
    let backup = path.with_extension("toml.bak");
    if !backup.exists() {
        std::fs::write(&backup, &original).map_err(|e| format!("写备份失败：{e}"))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, updated).map_err(|e| format!("写临时文件失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换配置失败：{e}"))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs<'a>(items: &'a [(&'a str, &'a str)]) -> Vec<(Option<&'a str>, &'a str)> {
        items.iter().map(|(title, url)| (Some(*title), *url)).collect()
    }

    /// The case that prompted this: one blog, a FeedBurner address and its own.
    #[test]
    fn one_blog_under_two_addresses_is_one_group() {
        let groups = find_duplicates(pairs(&[
            ("阮一峰的网络日志", "http://feeds.feedburner.com/ruanyifeng"),
            ("阮一峰的网络日志", "https://www.ruanyifeng.com/blog/atom.xml"),
            ("别的博客", "https://other.example/feed"),
        ]));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].keep, "https://www.ruanyifeng.com/blog/atom.xml");
        assert_eq!(groups[0].drop.len(), 1);
        assert_eq!(groups[0].drop[0].0, "http://feeds.feedburner.com/ruanyifeng");
        assert!(groups[0].drop[0].1.contains("转发服务"));
    }

    #[test]
    fn a_generic_title_is_not_an_identity() {
        assert!(find_duplicates(pairs(&[
            ("Home", "https://a.example/feed"),
            ("Home", "https://b.example/feed"),
            ("博客", "https://c.example/feed"),
            ("博客", "https://d.example/feed"),
            // Too short to mean anything either.
            ("AI", "https://e.example/feed"),
            ("AI", "https://f.example/feed"),
        ]))
        .is_empty());
    }

    #[test]
    fn https_and_the_shorter_address_win() {
        let groups = find_duplicates(pairs(&[
            ("Kubernetes Blog", "http://kubernetes.io/feed.xml"),
            ("Kubernetes Blog", "https://kubernetes.io/feed.xml"),
            ("Kubernetes Blog", "https://kubernetes.io/blog/feed.xml?ref=rss"),
        ]));
        assert_eq!(groups[0].keep, "https://kubernetes.io/feed.xml");
        assert_eq!(groups[0].drop.len(), 2);
    }

    #[test]
    fn the_same_url_twice_is_not_a_group() {
        assert!(
            find_duplicates(pairs(&[
                ("某博客", "https://x.example/feed"),
                ("某博客", "https://x.example/feed"),
            ]))
            .is_empty(),
            "the loader already collapses this"
        );
    }

    #[test]
    fn titles_are_compared_ignoring_case_and_spacing() {
        assert_eq!(normalise_title("Rust  Blog"), normalise_title("rust blog"));
        assert_eq!(
            find_duplicates(pairs(&[
                ("Rust Blog", "https://blog.rust-lang.org/feed.xml"),
                ("rust  blog", "http://blog.rust-lang.org/feed.xml"),
            ]))
            .len(),
            1
        );
    }

    #[test]
    fn a_source_without_a_title_is_left_alone() {
        assert!(
            find_duplicates(vec![
                (Some("有名字的博客"), "https://a.example/feed"),
                (None, "https://a.example/other"),
            ])
            .is_empty()
        );
    }
}

#[cfg(test)]
mod writer_tests {
    use super::remove_sources_from_file;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("artifeeds-dedupe-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("custom.toml")
    }

    /// A batch is one write per file, not one write per feed: the backup is the
    /// file as it was, and nothing is rewritten twice.
    #[test]
    fn removes_several_feeds_from_a_file_in_one_write() {
        let path = temp_file("batch");
        let original = "# mine\n\n\
            [[sources]]\ntitle = \"A\"\nurl = \"https://a.example/feed\"\nkey = \"https://a.example/feed\"\n\n\
            [[sources]]\ntitle = \"B\"\nurl = \"https://b.example/feed\"\nkey = \"https://b.example/feed\"\n\n\
            [[sources]]\ntitle = \"C\"\nurl = \"https://c.example/feed\"\nkey = \"https://c.example/feed\"\n";
        std::fs::write(&path, original).unwrap();

        let removed = remove_sources_from_file(
            path.to_str().unwrap(),
            &["https://a.example/feed".to_string(), "https://b.example/feed".to_string()],
        )
        .unwrap();
        assert_eq!(removed, 2);

        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["sources"].as_array().unwrap().len(), 1, "only C is left");
        assert!(text.starts_with("# mine"), "the reader's own comment survives");
        let backup = std::fs::read_to_string(path.with_extension("toml.bak")).unwrap();
        assert_eq!(backup, original, "the backup is the file before the edit");
        assert!(!path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn a_url_that_is_not_there_is_reported_not_ignored() {
        let path = temp_file("missing");
        std::fs::write(
            &path,
            "[[sources]]\nurl = \"https://a.example/feed\"\nkey = \"https://a.example/feed\"\n",
        )
        .unwrap();
        let err = remove_sources_from_file(path.to_str().unwrap(), &["https://nope.example/feed".to_string()])
            .unwrap_err();
        assert!(err.contains("找不到"), "{err}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("a.example"));
    }
}
