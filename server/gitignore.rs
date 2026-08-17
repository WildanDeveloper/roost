use regex::Regex;

/// A compiled gitignore pattern, mirroring `github.com/sabhiram/go-gitignore`
/// (the library wings uses for backup ignore patterns in archive.go).
#[derive(Debug, Clone)]
pub struct IgnorePattern {
    re: Regex,
    negate: bool,
}

/// Compile newline-separated gitignore lines into matchers.
/// Follows the exact transformation pipeline of go-gitignore
/// `getPatternFromLine` so ignore behavior matches wings.
pub fn compile(lines: &str) -> Vec<IgnorePattern> {
    let mut out = Vec::new();
    for line in lines.split('\n') {
        if let Some((re, negate)) = pattern_from_line(line) {
            out.push(IgnorePattern { re, negate });
        }
    }
    out
}

/// Returns true when the relative path (using `/` separators, no leading
/// slash) matches any pattern; later negated patterns can undo earlier
/// matches (go-gitignore MatchesPathHow).
pub fn matches_path(patterns: &[IgnorePattern], f: &str) -> bool {
    let f = f.replace('\\', "/");
    let mut matched = false;
    for p in patterns {
        if p.re.is_match(&f) {
            if !p.negate {
                matched = true;
            } else if matched {
                matched = false;
            }
        }
    }
    matched
}

fn pattern_from_line(line: &str) -> Option<(Regex, bool)> {
    let line = line.trim_end_matches('\r');
    if line.starts_with('#') {
        return None;
    }
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut negate = false;
    let mut line = line;
    if let Some(rest) = line.strip_prefix('!') {
        negate = true;
        line = rest;
    }
    // Handle escaped leading # or ! (go-gitignore rule 2/4 handling).
    if line.starts_with('#') || line.starts_with('!') {
        line = &line[1..];
    }
    let mut line = line.to_string();

    // "If we encounter a foo/*.blah in a folder, prepend the / char".
    if !line.starts_with('/')
        && Regex::new(r"([^/+])/.*\*.").ok()?.is_match(&line)
    {
        line.insert(0, '/');
    }

    // Handle escaping the "." char.
    line = line.replace('.', r"\.");

    const MAGIC_STAR: &str = "#$~";

    // Handle "/**/" usage.
    if let Some(stripped) = line.strip_prefix("/**/") {
        line = stripped.to_string();
    }
    line = Regex::new(r"/\*\*/")
        .ok()?
        .replace_all(&line, "(/|/.+/)")
        .into_owned();
    line = Regex::new(r"\*\*/")
        .ok()?
        .replace_all(&line, "(|.*/)")
        .into_owned();
    line = Regex::new(r"/\*\*")
        .ok()?
        .replace_all(&line, "(|/.*)")
        .into_owned();

    // Handle escaping the "*" char: protect escaped stars from the `*`
    // replacement below, then restore them (go-gitignore magicStar).
    line = line.replace(r"\*", r"\#$~");
    line = line.replace('*', "([^/]*)");

    // Handle escaping the "?" char.
    line = line.replace('?', r"\?");

    line = line.replace(MAGIC_STAR, "*");

    let expr = if line.ends_with('/') {
        format!("{line}(|.*)$")
    } else {
        format!("{line}(|/.*)$")
    };
    let expr = if expr.starts_with('/') {
        format!("^(|/){}", &expr[1..])
    } else {
        format!("^(|.*/){expr}")
    };

    Regex::new(&expr).ok().map(|re| (re, negate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(lines: &str, path: &str) -> bool {
        let patterns = compile(lines);
        matches_path(&patterns, path)
    }

    #[test]
    fn basic_paths() {
        let lines = "abc/def\na/b/c\nb";
        assert!(matches(lines, "abc/def/child"));
        assert!(matches(lines, "a/b/c/d"));
        assert!(!matches(lines, "abc"));
        assert!(!matches(lines, "def"));
        assert!(!matches(lines, "bd"));
    }

    #[test]
    fn negation() {
        let lines = "\n/*\n!/foo\n/foo/*\n!/foo/bar\n";
        assert!(matches(lines, "a"));
        assert!(matches(lines, "foo/baz"));
        assert!(!matches(lines, "foo"));
        assert!(!matches(lines, "/foo/bar"));
    }

    #[test]
    fn comments_and_spaces() {
        let lines = "\n#\n# A comment\n\n# Another comment\n\n\n    # Invalid Comment\n\nabc/def\n";
        assert!(!matches(lines, "abc/abc"));
        assert!(matches(lines, "abc/def"));
    }

    #[test]
    fn leading_slash() {
        let lines = "/a/b/c\nd/e/f\n/g";
        assert!(matches(lines, "a/b/c"));
        assert!(matches(lines, "a/b/c/d"));
        assert!(matches(lines, "d/e/f"));
        assert!(matches(lines, "g"));
    }

    #[test]
    fn leading_special_chars() {
        let lines = "\n# Comment\n\\#file.txt\n\\!file.txt\nfile.txt\n";
        assert!(matches(lines, "#file.txt"));
        assert!(matches(lines, "!file.txt"));
        assert!(matches(lines, "a/!file.txt"));
        assert!(matches(lines, "file.txt"));
        assert!(matches(lines, "a/file.txt"));
        assert!(!matches(lines, "file2.txt"));
    }

    #[test]
    fn all_files_in_dir() {
        let lines = "Documentation/*.html\n";
        assert!(matches(lines, "Documentation/git.html"));
        assert!(!matches(lines, "Documentation/ppc/ppc.html"));
        assert!(!matches(lines, "tools/perf/Documentation/perf.html"));
    }

    #[test]
    fn double_star() {
        let lines = "**/foo\nbar";
        assert!(matches(lines, "foo"));
        assert!(matches(lines, "baz/foo"));
        assert!(matches(lines, "bar"));
        assert!(matches(lines, "baz/bar"));
    }

    #[test]
    fn leading_slash_path() {
        let lines = "/*.c";
        assert!(matches(lines, "hello.c"));
        assert!(!matches(lines, "foo/hello.c"));
    }

    #[test]
    fn wildcard_files() {
        let lines = "*.swp\n/foo/*.wat\nbar/*.txt";
        assert!(matches(lines, "yo.swp"));
        assert!(matches(lines, "something/else/but/it/hasyo.swp"));
        assert!(matches(lines, "foo/bar.wat"));
        assert!(matches(lines, "/foo/something.wat"));
        assert!(matches(lines, "bar/something.txt"));
        assert!(matches(lines, "/bar/somethingelse.txt"));
        assert!(!matches(lines, "something/not/infoo/wat.wat"));
        assert!(!matches(lines, "something/not/infoo/wat.txt"));
    }

    #[test]
    fn preceding_slash() {
        let lines = "/foo\nbar/";
        assert!(matches(lines, "foo/bar.wat"));
        assert!(matches(lines, "/foo/something.txt"));
        assert!(matches(lines, "bar/something.txt"));
        assert!(matches(lines, "/bar/somethingelse.go"));
        assert!(matches(lines, "/boo/something/bar/boo.txt"));
        assert!(!matches(lines, "something/foo/something.txt"));
    }

    #[test]
    fn nested_dot_files() {
        let lines = "**/external/**/*.md\n**/external/**/*.json\n**/external/**/*.gzip\n**/external/**/.*ignore\n**/external/foobar/*.css\n**/external/barfoo/less\n**/external/barfoo/scss";
        assert!(matches(lines, "external/foobar/angular.foo.css"));
        assert!(matches(lines, "external/barfoo/.gitignore"));
        assert!(matches(lines, "external/barfoo/.bower.json"));
    }

    #[test]
    fn carriage_return() {
        let lines = "abc/def\r\na/b/c\r\nb\r";
        assert!(matches(lines, "abc/def/child"));
        assert!(matches(lines, "a/b/c/d"));
        assert!(!matches(lines, "abc"));
        assert!(!matches(lines, "def"));
        assert!(!matches(lines, "bd"));
    }
}