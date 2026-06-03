//! Shared synthetic data generators for the `ratio` and `bench_large` examples.
//!
//! (Lives in a subdirectory so Cargo does not treat it as its own example
//! target; both examples pull it in with `mod common;`.)

/// A deterministic **Wikipedia-XML-like** text generator — the shape of the Large
/// Text Compression Benchmark (`enwik8`/`enwik9`): `<page>` records carrying a
/// title, revision metadata, and a `<text>` body of mixed-case prose sprinkled
/// with `[[wiki links]]`, `{{templates}}`, `'''bold'''`, and `== headings ==`.
/// The repeated XML/markup structure feeds the match finder while the varied
/// prose exercises the entropy coder, so it stands in for real enwik text in the
/// benchmarks without needing the multi-hundred-MB fixture. Produces at least
/// `target` bytes (it stops after the page that crosses `target`), deterministic
/// from `seed`.
pub fn enwik_like(target: usize, seed: u64) -> Vec<u8> {
    // Capitalized tokens (titles, sentence starts, link targets) and lowercase
    // function words — a small vocabulary gives realistic word-level redundancy.
    const CAPS: &[&str] = &[
        "The", "In", "After", "During", "History", "Geography", "Science", "Music",
        "Europe", "America", "Asia", "Africa", "King", "Queen", "River", "Mountain",
        "City", "University", "Government", "Empire", "Republic", "War", "Battle",
        "Treaty", "Church", "Party", "Company", "Island", "Region", "Language",
    ];
    const WORDS: &[&str] = &[
        "the", "of", "and", "in", "to", "a", "is", "was", "for", "on", "as", "by",
        "with", "that", "from", "at", "an", "were", "are", "which", "also", "its",
        "this", "be", "has", "or", "but", "not", "they", "their", "had", "who",
        "one", "first", "after", "new", "used", "known", "called", "later", "time",
        "year", "century", "people", "between", "during", "while", "within", "such",
    ];
    const TEMPLATES: &[&str] =
        &["cite web", "cite book", "reflist", "main", "see also", "convert", "lang"];

    let mut s = seed | 1;
    let mut rnd = move || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (s >> 33) as usize
    };

    let mut out = Vec::with_capacity(target + 2048);
    let mut id = 1usize;
    while out.len() < target {
        let title = format!("{} {}", CAPS[rnd() % CAPS.len()], CAPS[rnd() % CAPS.len()]);
        out.extend_from_slice(
            format!(
                "  <page>\n    <title>{title}</title>\n    <ns>0</ns>\n    <id>{id}</id>\n    \
                 <revision>\n      <id>{}</id>\n      \
                 <timestamp>20{:02}-{:02}-{:02}T{:02}:{:02}:{:02}Z</timestamp>\n      \
                 <contributor>\n        <username>{}{}</username>\n      </contributor>\n      \
                 <comment>{} {}</comment>\n      <text xml:space=\"preserve\">",
                rnd() % 100_000_000,
                rnd() % 24, 1 + rnd() % 12, 1 + rnd() % 28, rnd() % 24, rnd() % 60, rnd() % 60,
                CAPS[rnd() % CAPS.len()], rnd() % 1000,
                WORDS[rnd() % WORDS.len()], WORDS[rnd() % WORDS.len()],
            )
            .as_bytes(),
        );
        let paras = 1 + rnd() % 4;
        for _ in 0..paras {
            if rnd() % 3 == 0 {
                out.extend_from_slice(format!("\n== {} ==\n", CAPS[rnd() % CAPS.len()]).as_bytes());
            }
            let sentences = 2 + rnd() % 5;
            for _ in 0..sentences {
                out.extend_from_slice(CAPS[rnd() % CAPS.len()].as_bytes()); // capitalized start
                let words = 6 + rnd() % 16;
                for _ in 0..words {
                    out.extend_from_slice(b" ");
                    match rnd() % 16 {
                        0 => out.extend_from_slice(format!("[[{}]]", CAPS[rnd() % CAPS.len()]).as_bytes()),
                        1 => out.extend_from_slice(
                            format!(
                                "[[{} {}|{}]]",
                                CAPS[rnd() % CAPS.len()],
                                CAPS[rnd() % CAPS.len()],
                                WORDS[rnd() % WORDS.len()]
                            )
                            .as_bytes(),
                        ),
                        2 => out.extend_from_slice(format!("{{{{{}}}}}", TEMPLATES[rnd() % TEMPLATES.len()]).as_bytes()),
                        3 => out.extend_from_slice(format!("'''{}'''", WORDS[rnd() % WORDS.len()]).as_bytes()),
                        _ => out.extend_from_slice(WORDS[rnd() % WORDS.len()].as_bytes()),
                    }
                }
                out.extend_from_slice(b". ");
            }
            out.extend_from_slice(b"\n\n");
        }
        out.extend_from_slice(b"</text>\n    </revision>\n  </page>\n");
        id += 1;
    }
    out
}
