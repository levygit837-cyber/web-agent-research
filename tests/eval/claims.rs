//! Claim precision (#123): is every bullet of the answer true to the pages
//! it cites?
//!
//! Recall (`judge`) asks whether the answer states the golden facts; this
//! asks the converse. The answer is split into bullets, each bullet is
//! matched to the pages it links that the run fetched (`pages.jsonl` of the
//! run's fixture dir), and one judge call per answered run gives every such
//! bullet a [`Verdict`] against the full text of its cited pages. A bullet
//! with no link, or whose links the run never fetched, is [`Verdict::Uncited`]
//! without a call. Eval-only; the rubric is in `docs/eval.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use web_agent_research::web::search::dedup_key;

use super::golden::CATEGORIES;
use super::judge::Judge;
use super::metrics::RunRow;

/// What a bullet is worth against its cited pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The cited pages state the main point and every specific detail.
    Supported,
    /// The main point is on the pages, a specific detail is not.
    Partly,
    /// The main point is not on the cited pages.
    Unsupported,
    /// A cited page says the opposite.
    Contradicted,
    /// No link, or only links to pages the run never fetched. Never sent
    /// to the judge.
    Uncited,
}

/// The claim pass result of one run: one verdict per bullet, in the order
/// [`split_bullets`] finds them in the answer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Claims {
    pub verdicts: Vec<Verdict>,
}

impl Claims {
    pub fn count(&self, verdict: Verdict) -> usize {
        self.verdicts.iter().filter(|v| **v == verdict).count()
    }
}

/// Share of the run's bullets with `verdict`; `None` when the run was not
/// claim-judged or has no bullet.
pub fn rate(row: &RunRow, verdict: Verdict) -> Option<f64> {
    let claims = row.claims.as_ref()?;
    (!claims.verdicts.is_empty())
        .then(|| claims.count(verdict) as f64 / claims.verdicts.len() as f64)
}

/// One bullet of the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bullet {
    /// The `##` title it sits under; empty before the first one.
    pub theme: String,
    /// The bullet text without its `(quote: "…")` suffix, links kept.
    pub text: String,
    /// The distinct URLs it links, in order.
    pub urls: Vec<String>,
    /// The verbatim quote the agent attached, when any.
    pub quote: Option<String>,
}

static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\]\((https?://(?:[^\s()]|\([^\s()]*\))+)\)").expect("link regex is valid")
});

const QUOTE_MARKER: &str = "(quote:";

/// Split the trailing `(quote: "…")` group off `raw`: the last such group,
/// in any letter case, closing at the first `)` after the last quote mark
/// (so a `)` inside the quoted words does not end it). Text after the group
/// (usually the final period) is kept.
fn split_quote(raw: &str) -> (String, Option<String>) {
    let Some(start) = raw.to_ascii_lowercase().rfind(QUOTE_MARKER) else {
        return (raw.trim().to_owned(), None);
    };
    let after = &raw[start + QUOTE_MARKER.len()..];
    let close = after
        .rfind(['"', '\u{201D}', '\u{201C}'])
        .and_then(|mark| {
            let rest = &after[mark..];
            let mark_len = rest.chars().next().map_or(1, char::len_utf8);
            rest[mark_len..].find(')').map(|p| mark + mark_len + p)
        })
        .or_else(|| after.rfind(')'));
    let (inner, tail) = match close {
        Some(close) => (&after[..close], &after[close + 1..]),
        None => (after, ""),
    };
    let quote = inner
        .trim()
        .trim_matches(['"', '\u{201C}', '\u{201D}'])
        .trim();
    // Punctuation right after the group (`).`) stays attached to the text;
    // words after a space keep one space.
    let gap = if tail.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let text = format!("{}{gap}{}", raw[..start].trim_end(), tail.trim());
    (
        text.trim().to_owned(),
        (!quote.is_empty()).then(|| quote.to_owned()),
    )
}

fn bullet(theme: &str, raw: &str) -> Bullet {
    let (text, quote) = split_quote(raw);
    let mut urls: Vec<String> = Vec::new();
    for caps in LINK.captures_iter(&text) {
        let url = caps[1].to_owned();
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    Bullet {
        theme: theme.to_owned(),
        text,
        urls,
        quote,
    }
}

/// The bullets of a rendered answer (`render` output): the `-`/`*` lines
/// under the `##` themes, up to the `Sources:` list. The summary paragraph
/// is not a bullet. An indented line continues the bullet above it.
pub fn split_bullets(answer: &str) -> Vec<Bullet> {
    let mut raws: Vec<(String, String)> = Vec::new();
    let mut theme = String::new();
    for line in answer.lines() {
        if line.trim_end() == "Sources:" {
            break;
        }
        if let Some(title) = line.strip_prefix("## ") {
            title.trim().clone_into(&mut theme);
        } else if let Some(text) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            raws.push((theme.clone(), text.trim().to_owned()));
        } else if line.starts_with([' ', '\t']) && !line.trim().is_empty() {
            if let Some((_, text)) = raws.last_mut() {
                text.push(' ');
                text.push_str(line.trim());
            }
        }
    }
    raws.iter().map(|(theme, raw)| bullet(theme, raw)).collect()
}

/// The recorded pages of one goal: `<fixtures>/<goal id>/pages.jsonl`.
#[derive(Debug, Default)]
pub struct Pages {
    /// `(requested url, cleaned markdown)`, one per fetched page.
    texts: Vec<(String, String)>,
    /// `dedup_key` of the requested and the final URL -> index in `texts`.
    index: HashMap<String, usize>,
}

#[derive(Deserialize)]
struct PageLine {
    url: String,
    outcome: String,
    evidence: Option<PageEvidence>,
}

#[derive(Deserialize)]
struct PageEvidence {
    source_url: String,
    markdown: String,
}

impl Pages {
    /// Parse a `pages.jsonl`; failed fetches are not pages. The first
    /// recording of a URL wins, like the replay fixture.
    pub fn parse(jsonl: &str) -> Result<Self, String> {
        let mut pages = Self::default();
        for (number, line) in jsonl.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: PageLine = serde_json::from_str(line)
                .map_err(|err| format!("pages.jsonl line {}: {err}", number + 1))?;
            let (true, Some(evidence)) = (entry.outcome == "page", entry.evidence) else {
                continue;
            };
            let keys = [dedup_key(&entry.url), dedup_key(&evidence.source_url)];
            if keys.iter().all(|key| pages.index.contains_key(key)) {
                continue;
            }
            pages.texts.push((entry.url, evidence.markdown));
            let at = pages.texts.len() - 1;
            for key in keys {
                pages.index.entry(key).or_insert(at);
            }
        }
        Ok(pages)
    }

    fn lookup(&self, url: &str) -> Option<usize> {
        self.index.get(&dedup_key(url)).copied()
    }
}

/// Fixture root with one lazily loaded [`Pages`] per goal.
pub struct Fixtures {
    root: PathBuf,
    loaded: HashMap<String, Pages>,
}

impl Fixtures {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            loaded: HashMap::new(),
        }
    }

    pub fn pages(&mut self, goal_id: &str) -> Result<&Pages, String> {
        if !self.loaded.contains_key(goal_id) {
            let path: PathBuf = [
                self.root.as_path(),
                Path::new(goal_id),
                Path::new("pages.jsonl"),
            ]
            .iter()
            .collect();
            let text = std::fs::read_to_string(&path).map_err(|err| {
                format!(
                    "{}: {err} (pass --fixtures with the recorded pages)",
                    path.display()
                )
            })?;
            let pages = Pages::parse(&text).map_err(|err| format!("{}: {err}", path.display()))?;
            self.loaded.insert(goal_id.to_owned(), pages);
        }
        Ok(&self.loaded[goal_id])
    }
}

/// Per-page and per-call caps on the page text sent to the judge, in
/// characters. A page over its cap is cut and says so.
const PAGE_CHARS_MAX: usize = 150_000;
const TOTAL_CHARS_MAX: usize = 500_000;

/// What to ask the judge for one run.
#[derive(Debug)]
pub struct Plan {
    pub bullets: Vec<Bullet>,
    /// Per bullet: indices into `pages` of the fetched pages it cites.
    pub cites: Vec<Vec<usize>>,
    /// The cited, fetched pages: `(url, text)`.
    pub pages: Vec<(String, String)>,
}

impl Plan {
    /// Indices of the bullets the judge must check.
    pub fn checked(&self) -> Vec<usize> {
        (0..self.bullets.len())
            .filter(|i| !self.cites[*i].is_empty())
            .collect()
    }
}

/// Match every bullet of `answer` to the recorded pages it cites.
pub fn plan(answer: &str, recorded: &Pages) -> Plan {
    let bullets = split_bullets(answer);
    let mut used: Vec<usize> = Vec::new();
    let mut cites = Vec::with_capacity(bullets.len());
    for bullet in &bullets {
        let mut mine: Vec<usize> = Vec::new();
        for url in &bullet.urls {
            let Some(page) = recorded.lookup(url) else {
                continue;
            };
            let local = used.iter().position(|p| *p == page).unwrap_or_else(|| {
                used.push(page);
                used.len() - 1
            });
            if !mine.contains(&local) {
                mine.push(local);
            }
        }
        cites.push(mine);
    }
    let pages = used.iter().map(|p| recorded.texts[*p].clone()).collect();
    Plan {
        bullets,
        cites,
        pages,
    }
}

fn clip(text: &str, cap: usize) -> String {
    match text.char_indices().nth(cap) {
        Some((end, _)) => format!("{}\n[page cut here]", &text[..end]),
        None => text.to_owned(),
    }
}

const SYSTEM: &str = "You check the claims of a web research answer against the pages it cites. You judge only from the page text you are given, never from your own knowledge. You are strict and literal. You reply with one JSON object and nothing else.";

/// The claim-check prompt for the checked bullets of `plan`.
pub fn prompt(goal: &str, plan: &Plan) -> String {
    let cap = (TOTAL_CHARS_MAX / plan.pages.len().max(1)).min(PAGE_CHARS_MAX);
    let mut pages = String::new();
    for (index, (url, text)) in plan.pages.iter().enumerate() {
        pages.push_str(&format!(
            "<<<PAGE P{n} {url}\n{text}\nPAGE>>>\n\n",
            n = index + 1,
            text = clip(text, cap)
        ));
    }
    let checked = plan.checked();
    let mut bullets = String::new();
    for (n, index) in checked.iter().enumerate() {
        let bullet = &plan.bullets[*index];
        let cites: Vec<String> = plan.cites[*index]
            .iter()
            .map(|page| format!("P{}", page + 1))
            .collect();
        bullets.push_str(&format!(
            "{}. [{}] {}\n   cites: {}\n",
            n + 1,
            bullet.theme,
            bullet.text,
            cites.join(", ")
        ));
    }
    format!(
        "Research question:\n{goal}\n\nFetched pages:\n\n{pages}Bullets of the answer, each with the pages it cites:\n{bullets}\n\
For each bullet, judge its claim against the text of the pages it cites (together, when it cites several), with one verdict:\n\
- \"supported\": the cited pages state the bullet's main point and every specific detail it asserts (names, numbers, versions, flags, signatures, code).\n\
- \"partly\": the cited pages state the main point, but at least one specific detail the bullet asserts is not on them.\n\
- \"unsupported\": the main point is not on the cited pages (they are silent on it or about something else).\n\
- \"contradicted\": a cited page says the opposite of the bullet.\n\
A paraphrase or a translation is fine when the facts and values are the same. Judge the bullet text, not the link titles. Ignore the research question except as context.\n\n\
Reply with exactly this JSON shape, one verdict per bullet in order:\n\
{{\"verdicts\": [\"supported\" | \"partly\" | \"unsupported\" | \"contradicted\", ...]}}"
    )
}

#[derive(Deserialize)]
struct Reply {
    verdicts: Vec<Verdict>,
}

/// Parse the judge's reply: the outermost `{…}` of `text`, with exactly
/// `expected` verdicts, none of them `uncited` (that one is not the
/// judge's to give).
pub fn parse(text: &str, expected: usize) -> Result<Vec<Verdict>, String> {
    let start = text.find('{').ok_or("claim reply has no JSON object")?;
    let end = text.rfind('}').ok_or("claim reply has no JSON object")?;
    let reply: Reply = serde_json::from_str(&text[start..=end])
        .map_err(|err| format!("claim reply: {err}: {text}"))?;
    if reply.verdicts.len() != expected {
        return Err(format!(
            "judge returned {} verdicts, expected {expected}",
            reply.verdicts.len()
        ));
    }
    if reply.verdicts.contains(&Verdict::Uncited) {
        return Err("judge returned \"uncited\"".to_owned());
    }
    Ok(reply.verdicts)
}

/// Verdicts of all bullets: the judge's for the checked ones, `uncited`
/// for the rest.
pub fn assemble(plan: &Plan, judged: &[Verdict]) -> Claims {
    let mut judged = judged.iter();
    let verdicts = (0..plan.bullets.len())
        .map(|i| {
            if plan.cites[i].is_empty() {
                Verdict::Uncited
            } else {
                *judged.next().expect("one verdict per checked bullet")
            }
        })
        .collect();
    Claims { verdicts }
}

/// The claim pass of one answered run: at most one judge call (two when the
/// first reply does not parse), none when no bullet cites a fetched page.
pub async fn check(
    judge: &Judge,
    fixtures: &mut Fixtures,
    goal_id: &str,
    goal: &str,
    answer: &str,
) -> Result<Claims, String> {
    let plan = plan(answer, fixtures.pages(goal_id)?);
    let checked = plan.checked().len();
    if checked == 0 {
        return Ok(assemble(&plan, &[]));
    }
    let prompt = prompt(goal, &plan);
    let mut last = String::new();
    for _ in 0..2 {
        let reply = judge.ask(SYSTEM, &prompt).await?;
        match parse(&reply, checked) {
            Ok(judged) => return Ok(assemble(&plan, &judged)),
            Err(err) => last = err,
        }
    }
    Err(last)
}

fn pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        "-".to_owned()
    } else {
        format!("{:.0}%", 100.0 * part as f64 / whole as f64)
    }
}

/// Markdown table of the claim metrics, one row per category in
/// [`CATEGORIES`] order, then `all`; bullets are pooled over the claim-judged
/// runs. Empty when no run was claim-judged.
pub fn table(rows: &[RunRow]) -> String {
    if rows.iter().all(|row| row.claims.is_none()) {
        return String::new();
    }
    let mut out = String::from(
        "| category | judged runs | bullets | claim precision | partly | unsupported | contradicted | uncited |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    let groups = CATEGORIES
        .iter()
        .map(|category| (*category, Some(*category)))
        .chain(std::iter::once(("all", None)));
    for (label, filter) in groups {
        let claims: Vec<&Claims> = rows
            .iter()
            .filter(|row| filter.is_none_or(|category| row.category == category))
            .filter_map(|row| row.claims.as_ref())
            .collect();
        if claims.is_empty() {
            continue;
        }
        let bullets: usize = claims.iter().map(|c| c.verdicts.len()).sum();
        let of = |verdict| pct(claims.iter().map(|c| c.count(verdict)).sum(), bullets);
        out.push_str(&format!(
            "| {label} | {} | {bullets} | {} | {} | {} | {} | {} |\n",
            claims.len(),
            of(Verdict::Supported),
            of(Verdict::Partly),
            of(Verdict::Unsupported),
            of(Verdict::Contradicted),
            of(Verdict::Uncited),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "Summary line ([S](https://s.example/)).\n\n## Setup\n\n- Enable `derive` ([Docs](https://docs.example/a)) (quote: \"features = [\\\"derive\\\"]\").\n- Two pages ([A](https://docs.example/a) and [B](https://www.b.example/x/)) with a repeat [A](https://docs.example/a).\n\n## Limits\n\n- Parens in url ([W](https://w.example/Foo_(bar))) (Quote: “curly (nested) quote”) end.\n  continued here\n* Star bullet with no link.\n\nSources:\n1. [Docs](https://docs.example/a)\n- not a bullet\n";

    #[test]
    fn the_split_keeps_themes_links_and_strips_the_quote_suffix() {
        let bullets = split_bullets(ANSWER);
        assert_eq!(bullets.len(), 4, "{bullets:#?}");
        assert_eq!(bullets[0].theme, "Setup");
        assert_eq!(
            bullets[0].text,
            "Enable `derive` ([Docs](https://docs.example/a))."
        );
        assert_eq!(
            bullets[0].quote.as_deref(),
            Some("features = [\\\"derive\\\"]")
        );
        assert_eq!(bullets[0].urls, ["https://docs.example/a"]);
        assert_eq!(
            bullets[1].urls,
            ["https://docs.example/a", "https://www.b.example/x/"]
        );
        assert_eq!(bullets[1].quote, None);
        assert_eq!(bullets[2].theme, "Limits");
        assert_eq!(bullets[2].urls, ["https://w.example/Foo_(bar)"]);
        assert_eq!(bullets[2].quote.as_deref(), Some("curly (nested) quote"));
        assert_eq!(
            bullets[2].text,
            "Parens in url ([W](https://w.example/Foo_(bar))) end. continued here"
        );
        assert!(bullets[3].urls.is_empty());
        assert_eq!(bullets[3].text, "Star bullet with no link.");
    }

    #[test]
    fn an_answer_without_themes_has_no_bullets() {
        assert!(
            split_bullets("Just a summary.\n\nSources:\n1. [A](https://a.example/)\n").is_empty()
        );
        assert!(split_bullets("").is_empty());
    }

    fn page(url: &str, final_url: &str, text: &str) -> String {
        format!(
            "{{\"key\":\"k\",\"url\":\"{url}\",\"outcome\":\"page\",\"evidence\":{{\"source_url\":\"{final_url}\",\"collected_at\":\"t\",\"markdown\":\"{text}\"}}}}\n"
        )
    }

    fn recorded() -> Pages {
        let mut jsonl = page(
            "https://docs.example/a",
            "https://docs.example/a",
            "derive text",
        );
        jsonl.push_str(&page(
            "https://b.example/old",
            "https://b.example/x",
            "b text",
        ));
        jsonl.push_str(
            "{\"key\":\"k\",\"url\":\"https://dead.example/\",\"outcome\":\"failed\",\"reason\":\"x\",\"kind\":\"execution\"}\n",
        );
        Pages::parse(&jsonl).expect("parses")
    }

    #[test]
    fn bullets_match_pages_by_normalized_url_and_final_url() {
        let plan = plan(ANSWER, &recorded());
        // Bullet 2 cites docs.example/a and www.b.example/x/ (b's final URL).
        assert_eq!(plan.cites, [vec![0], vec![0, 1], vec![], vec![]]);
        assert_eq!(plan.checked(), [0, 1]);
        assert_eq!(plan.pages.len(), 2);
        assert_eq!(plan.pages[1].0, "https://b.example/old");
    }

    #[test]
    fn a_bad_pages_line_names_its_number() {
        let err = Pages::parse("\n{\"nope\":1}\n").expect_err("rejected");
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn the_prompt_numbers_checked_bullets_and_pages_and_clips_long_pages() {
        let plan = plan(ANSWER, &recorded());
        let text = prompt("What?", &plan);
        assert!(
            text.contains("<<<PAGE P1 https://docs.example/a\nderive text\nPAGE>>>"),
            "{text}"
        );
        assert!(
            text.contains("<<<PAGE P2 https://b.example/old\n"),
            "{text}"
        );
        assert!(text.contains("1. [Setup] Enable"), "{text}");
        assert!(text.contains("   cites: P1, P2\n"), "{text}");
        assert!(!text.contains("Star bullet"), "{text}");
        assert_eq!(clip("héllo wörld", 5), "héllo\n[page cut here]");
        assert_eq!(clip("short", 5), "short");
    }

    #[test]
    fn a_reply_needs_one_judge_verdict_per_checked_bullet() {
        let ok = parse(
            "```json\n{\"verdicts\": [\"supported\", \"partly\"]}\n```",
            2,
        )
        .expect("ok");
        assert_eq!(ok, [Verdict::Supported, Verdict::Partly]);
        assert!(parse("{\"verdicts\": [\"supported\"]}", 2).is_err());
        assert!(parse("{\"verdicts\": [\"maybe\"]}", 1).is_err());
        assert!(parse("{\"verdicts\": [\"uncited\"]}", 1).is_err());
        assert!(parse("no json", 1).is_err());
    }

    #[test]
    fn uncited_bullets_slot_between_the_judged_ones() {
        let plan = plan(ANSWER, &recorded());
        let claims = assemble(&plan, &[Verdict::Supported, Verdict::Contradicted]);
        assert_eq!(
            claims.verdicts,
            [
                Verdict::Supported,
                Verdict::Contradicted,
                Verdict::Uncited,
                Verdict::Uncited
            ]
        );
        assert_eq!(claims.count(Verdict::Uncited), 2);
    }

    fn row(category: &str, verdicts: Option<&[Verdict]>) -> RunRow {
        RunRow {
            goal_id: format!("{category}-01"),
            category: category.to_owned(),
            repeat: 1,
            metrics: super::super::metrics::Metrics::from_output(7, "", "").expect("failed run"),
            wall_ms: 0,
            answer: None,
            error: None,
            judge: None,
            claims: verdicts.map(|v| Claims {
                verdicts: v.to_vec(),
            }),
        }
    }

    #[test]
    fn rates_divide_by_all_bullets_and_are_missing_without_a_claim_pass() {
        use Verdict::*;
        let judged = row("how_to", Some(&[Supported, Supported, Partly, Uncited]));
        assert_eq!(rate(&judged, Supported), Some(0.5));
        assert_eq!(rate(&judged, Uncited), Some(0.25));
        assert_eq!(rate(&judged, Contradicted), Some(0.0));
        assert_eq!(rate(&row("how_to", None), Supported), None);
        assert_eq!(rate(&row("how_to", Some(&[])), Supported), None);
    }

    #[test]
    fn the_table_pools_bullets_per_category_and_hides_when_unjudged() {
        use Verdict::*;
        assert_eq!(table(&[row("how_to", None)]), "");
        let rows = [
            row("how_to", Some(&[Supported, Supported, Partly, Uncited])),
            row("how_to", Some(&[Unsupported, Supported])),
            row("pt_br", Some(&[Contradicted])),
            row("pt_br", None),
        ];
        let text = table(&rows);
        assert!(
            text.contains("| how_to | 2 | 6 | 50% | 17% | 17% | 0% | 17% |"),
            "{text}"
        );
        assert!(
            text.contains("| pt_br | 1 | 1 | 0% | 0% | 0% | 100% | 0% |"),
            "{text}"
        );
        assert!(
            text.contains("| all | 3 | 7 | 43% | 14% | 14% | 14% | 14% |"),
            "{text}"
        );
        assert!(!text.contains("| crate_docs |"), "{text}");
    }
}
