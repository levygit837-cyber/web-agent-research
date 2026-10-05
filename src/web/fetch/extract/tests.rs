use super::*;
use crate::web::fetch::fetcher::main_content;

const SKIP: &[&str] = &[
    "head", "script", "style", "noscript", "template", "svg", "iframe", "nav", "footer", "form",
];
const MIN_CHARS: usize = 200;

fn prose(words: usize) -> String {
    "content words fill the article body here ".repeat(words / 7 + 1)
}

fn ex(html: &str, thread_outside: bool) -> Extraction {
    Extraction {
        html: html.to_owned(),
        text_share: 1.0,
        thread_outside,
    }
}

#[test]
fn gate_table() {
    let body = prose(200);
    let baseline_plain = format!("Sidebar link list\n\n{body}");
    let fenced = "```rust\nfn main() {}\n```";
    let baseline_fenced = format!("{fenced}\n\n{body}");
    let row = "| a | b |";
    let baseline_table = format!("{row}\n| --- | --- |\n| 1 | 2 |\n\n{body}");
    let unclosed = format!("{body}\n\n```sh\necho tail");
    let table_missing_row = baseline_table.replace("| 1 | 2 |", "");

    struct Case<'a> {
        name: &'a str,
        baseline: &'a str,
        extracted: Option<(Extraction, &'a str)>,
        keep: bool,
    }
    let cases = [
        Case {
            name: "extractor failed",
            baseline: &baseline_plain,
            extracted: None,
            keep: false,
        },
        Case {
            name: "drops only chrome",
            baseline: &baseline_plain,
            extracted: Some((ex("", false), &body)),
            keep: true,
        },
        Case {
            name: "below length fraction",
            baseline: &baseline_plain,
            extracted: Some((ex("", false), &body[..body.len() / 3])),
            keep: false,
        },
        Case {
            name: "below min_markdown_chars",
            baseline: "short page with a short sidebar",
            extracted: Some((ex("", false), "short page")),
            keep: false,
        },
        Case {
            name: "keeps fenced block",
            baseline: &baseline_fenced,
            extracted: Some((ex("", false), &baseline_fenced)),
            keep: true,
        },
        Case {
            name: "loses fenced block",
            baseline: &baseline_fenced,
            extracted: Some((ex("", false), &body)),
            keep: false,
        },
        Case {
            name: "loses unclosed trailing fence",
            baseline: &unclosed,
            extracted: Some((ex("", false), &body)),
            keep: false,
        },
        Case {
            name: "keeps table rows",
            baseline: &baseline_table,
            extracted: Some((ex("", false), &baseline_table)),
            keep: true,
        },
        Case {
            name: "loses a table row",
            baseline: &baseline_table,
            extracted: Some((ex("", false), &table_missing_row)),
            keep: false,
        },
        Case {
            name: "thread answers outside subtree",
            baseline: &baseline_plain,
            extracted: Some((ex("", true), &body)),
            keep: false,
        },
    ];
    for case in cases {
        let got = keep_extracted(
            case.baseline,
            case.extracted.as_ref().map(|(e, md)| (e, *md)),
            MIN_CHARS,
        );
        assert_eq!(got, case.keep, "{}", case.name);
    }
}

#[test]
fn selection_order_main_then_role_then_single_article() {
    let body = prose(100);
    let page = |inner: &str| {
        format!(
            "<html><body><div class=side>{}</div>{inner}</body></html>",
            "x ".repeat(10)
        )
    };
    let main = extract(&page(&format!("<main><p>{body}</p></main>")), SKIP).unwrap();
    assert!(main.html.starts_with("<main"));
    let role = extract(
        &page(&format!(
            "<div role=\"main\"><p>{body}</p></div><article>a</article>"
        )),
        SKIP,
    )
    .unwrap();
    assert!(role.html.starts_with("<div"), "{}", role.html);
    let article = extract(&page(&format!("<article><p>{body}</p></article>")), SKIP).unwrap();
    assert!(article.html.starts_with("<article"));
    let two_articles = page(&format!(
        "<article><p>{body}</p></article><article><p>{body}</p></article>"
    ));
    assert_eq!(
        extract(&two_articles, SKIP),
        None,
        "several articles: no pick"
    );
    let two_mains = page(&format!(
        "<main><p>{body}</p></main><main><p>{body}</p></main>"
    ));
    assert_eq!(extract(&two_mains, SKIP), None, "several mains: no pick");
}

#[test]
fn small_subtree_is_not_selected() {
    let html = format!(
        "<html><body><div>{}</div><main><p>tiny</p></main></body></html>",
        prose(200)
    );
    assert_eq!(extract(&html, SKIP), None);
}

#[test]
fn qa_page_with_answers_outside_main_is_rejected() {
    let question = prose(240);
    let answer = format!("<div class=\"answer\"><p>{}</p></div>", prose(80));
    let comment = "<div class=\"comment\"><p>a comment on the question</p></div>";
    let html = format!(
        "<html><body><main><h1>Q</h1><p>{question}</p></main>{answer}{comment}</body></html>"
    );
    let extraction = extract(&html, SKIP).expect("main holds most text");
    assert!(extraction.thread_outside);
    let page = main_content(&html, MIN_CHARS);
    assert!(!page.kept, "answers outside main must keep the baseline");

    // The same answers inside the subtree are fine.
    let inside = format!(
        "<html><body><nav>n</nav><main><h1>Q</h1><p>{question}</p>{answer}{comment}</main></body></html>"
    );
    assert!(!extract(&inside, SKIP).unwrap().thread_outside);
}

#[test]
fn fenced_blocks_follow_clean_markdown_fence_rules() {
    let md = "a\n````md\n```rust\nx\n```\n````\nb\n~~~\ny\n~~~";
    assert_eq!(
        fenced_blocks(md),
        vec![
            "````md\n```rust\nx\n```\n````".to_owned(),
            "~~~\ny\n~~~".to_owned()
        ]
    );
}

/// Every saved real page (`tests/fixtures/pages/`): whatever the gate
/// decides, the delivered markdown keeps every fenced block and table row
/// of the baseline. Prints the per-fixture measurement (`--nocapture`).
#[test]
fn every_fixture_keeps_every_baseline_fenced_block_and_table_row() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pages");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixture dir")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "html"))
        .collect();
    names.sort();
    assert!(
        names.len() >= 30,
        "need 30+ fixtures, found {}",
        names.len()
    );
    println!("fixture | baseline chars | extracted chars | ratio | text share | decision");
    for path in names {
        let html = std::fs::read_to_string(&path).expect("fixture reads");
        let page = main_content(&html, MIN_CHARS);
        let delivered = match (&page.extracted, page.kept) {
            (Some(extracted), true) => extracted.as_str(),
            _ => page.baseline.as_str(),
        };
        let name = path.file_stem().unwrap().to_string_lossy();
        for block in fenced_blocks(&page.baseline) {
            assert!(
                delivered.contains(&block),
                "{name}: lost fenced block {block:?}"
            );
        }
        for row in table_rows(&page.baseline) {
            assert!(delivered.contains(row), "{name}: lost table row {row:?}");
        }
        let base = page.baseline.chars().count();
        let ext = page.extracted.as_ref().map(|md| md.chars().count());
        let share = extract(&html, SKIP).map(|e| e.text_share);
        println!(
            "{name} | {base} | {} | {} | {} | {}",
            ext.map_or("-".to_owned(), |n| n.to_string()),
            ext.map_or("-".to_owned(), |n| format!(
                "{:.2}",
                n as f64 / base.max(1) as f64
            )),
            share.map_or("-".to_owned(), |s| format!("{s:.2}")),
            if page.kept { "extracted" } else { "baseline" }
        );
    }
}
