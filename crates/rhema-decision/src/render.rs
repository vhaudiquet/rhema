//! The text a `decide` command prints: a header naming who answered,
//! then one line per question.

use crate::answer::Answer;
use crate::response::DecisionResponse;

/// The most bytes the whole rendering may carry. Lines past it drop
/// with a notice, so the model reads the cap's name instead of the
/// shell tool's stdout cap silently eating the tail.
pub const MAX_RENDER_BYTES: usize = 8 * 1024;

/// The probability from which a noul reads as a yes in words.
pub const YES_ABOVE: f64 = 0.9;
/// The probability under which a noul reads as a no in words.
pub const NO_BELOW: f64 = 0.1;

/// The most options a choice or score line lists before the tail
/// folds into a count. The pick and its probability always show; a
/// 255-option distribution is a file, not a line.
pub const MAX_LISTED: usize = 12;

/// Renders one response. The header names the provider kind (`jev`)
/// and the model that answered, so a session whose answers look
/// wrong can be traced to a vendor; the token counts follow, since
/// the vendors bill per input token, and the charge when the vendor
/// reports one (OpenRouter does).
pub fn render(response: &DecisionResponse, kind: &str) -> String {
    let charge = response
        .usage
        .cost
        .map(|cost| format!(", {cost:.6} credits"))
        .unwrap_or_default();
    let mut out = format!(
        "{kind} {model}: {count} answer(s), {input} in / {output} out{charge}\n",
        model = response.model,
        count = response.answers.len(),
        input = response.usage.input_tokens,
        output = response.usage.output_tokens,
    );
    if response.answers.is_empty() {
        return out;
    }
    out.push('\n');
    // Counted, not derived from `out.lines()`: the header and its blank
    // line are in there, so a lone oversized answer made the subtraction
    // `1 - 2`, a panic in debug and a wrapped count in release.
    let mut rendered = 0usize;
    for (question, answer) in &response.answers {
        let line = format!("{question}: {}\n", render_answer(answer));
        if out.len() + line.len() > MAX_RENDER_BYTES {
            break;
        }
        out.push_str(&line);
        rendered += 1;
    }
    let dropped = response.answers.len() - rendered;
    if dropped > 0 {
        out.push_str(&format!(
            "[{dropped} more answer(s) dropped: the {MAX_RENDER_BYTES} byte render cap]\n"
        ));
    }
    out
}

/// One answer on one line: the pick first, the distribution in
/// brackets, confidence last where the model reports one.
pub fn render_answer(answer: &Answer) -> String {
    match answer {
        Answer::Noul { noul } => format!("{noul:.2} {}", verdict(*noul)),
        Answer::Choice {
            choice,
            confidence,
            probabilities,
        } => {
            let mut sorted: Vec<(&String, &f64)> = probabilities.iter().collect();
            // Most likely first: the reader wants the runner-up, not
            // the alphabet.
            sorted.sort_by(|a, b| b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0)));
            format!(
                "{choice} {dist} confidence {confidence:.2}",
                dist = distribution(&sorted)
            )
        }
        Answer::Score {
            score,
            confidence,
            probabilities,
            ..
        } => {
            let mut sorted: Vec<(&String, &f64)> = probabilities.iter().collect();
            // By level: the keys are the levels' numbers as text, and
            // "10" sorts before "2" as text.
            sorted.sort_by_key(|(level, _)| level.parse::<u32>().unwrap_or(u32::MAX));
            format!(
                "{score:.2} on 0-{top} {dist} confidence {confidence:.2}",
                top = sorted.len().saturating_sub(1),
                dist = distribution(&sorted)
            )
        }
        Answer::Refusal => "refused by the provider: not answered".to_owned(),
        Answer::Other => "an answer kind this seam does not model".to_owned(),
    }
}

/// The distribution in brackets, the first [`MAX_LISTED`] entries
/// then a count for the rest.
fn distribution(sorted: &[(&String, &f64)]) -> String {
    let mut parts: Vec<String> = sorted
        .iter()
        .take(MAX_LISTED)
        .map(|(id, p)| format!("{id} {p:.2}"))
        .collect();
    if sorted.len() > MAX_LISTED {
        parts.push(format!("{} more", sorted.len() - MAX_LISTED));
    }
    format!("[{}]", parts.join(", "))
}

/// The word for a noul: yes above [`YES_ABOVE`], no under
/// [`NO_BELOW`], uncertain between. The number rides along either way.
fn verdict(noul: f64) -> &'static str {
    if noul >= YES_ABOVE {
        "yes"
    } else if noul <= NO_BELOW {
        "no"
    } else {
        "uncertain"
    }
}

/// The response as pretty JSON, for `--json`: the vendor's shape,
/// nothing of this crate's rendering.
pub fn render_json(response: &DecisionResponse) -> String {
    serde_json::to_string_pretty(response).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn response(answers: BTreeMap<String, Answer>) -> DecisionResponse {
        DecisionResponse {
            model: "jev-1.13.0".into(),
            answers,
            usage: crate::response::Usage {
                input_tokens: 328,
                cost: None,
                output_tokens: 34,
            },
        }
    }

    #[test]
    fn the_header_names_the_entry_the_model_and_the_usage() {
        let response = response(BTreeMap::from([(
            "q".to_owned(),
            Answer::Noul { noul: 0.5 },
        )]));
        let out = render(&response, "jev");
        assert!(
            out.starts_with("jev jev-1.13.0: 1 answer(s), 328 in / 34 out\n\n"),
            "{out}"
        );
    }

    #[test]
    fn the_three_kinds_each_render_on_one_line() {
        let response = response(BTreeMap::from([
            ("urgent".to_owned(), Answer::Noul { noul: 0.97 }),
            ("risky".to_owned(), Answer::Noul { noul: 0.42 }),
            ("fine".to_owned(), Answer::Noul { noul: 0.03 }),
            (
                "team".to_owned(),
                Answer::Choice {
                    choice: "billing".into(),
                    confidence: 0.86,
                    probabilities: BTreeMap::from([
                        ("technical".to_owned(), 0.06),
                        ("billing".to_owned(), 0.91),
                        ("sales".to_owned(), 0.03),
                    ]),
                },
            ),
            (
                "severity".to_owned(),
                Answer::Score {
                    score: 1.43,
                    confidence: 0.35,
                    legend: BTreeMap::from([("0".to_owned(), json!(" Cosmetic "))]),
                    probabilities: BTreeMap::from([
                        ("2".to_owned(), 0.43),
                        ("0".to_owned(), 0.0),
                        ("1".to_owned(), 0.57),
                    ]),
                },
            ),
            ("odd".to_owned(), Answer::Other),
            ("flagged".to_owned(), Answer::Refusal),
        ]));
        let out = render(&response, "jev");
        assert!(out.contains("\nurgent: 0.97 yes\n"), "{out}");
        assert!(out.contains("\nrisky: 0.42 uncertain\n"), "{out}");
        assert!(out.contains("\nfine: 0.03 no\n"), "{out}");
        // Most likely first, the pick leads.
        assert!(
            out.contains(
                "\nteam: billing [billing 0.91, technical 0.06, sales 0.03] confidence 0.86\n"
            ),
            "{out}"
        );
        // Levels in numeric order, the top of the scale stated.
        assert!(
            out.contains("\nseverity: 1.43 on 0-2 [0 0.00, 1 0.57, 2 0.43] confidence 0.35\n"),
            "{out}"
        );
        assert!(
            out.contains("\nodd: an answer kind this seam does not model\n"),
            "{out}"
        );
        // A refused question says so; it never reads as a no.
        assert!(
            out.contains("\nflagged: refused by the provider: not answered\n"),
            "{out}"
        );
    }

    #[test]
    fn a_reported_charge_ends_the_header() {
        let response = response(BTreeMap::new());
        let with_cost = DecisionResponse {
            usage: crate::response::Usage {
                cost: Some(0.0000384),
                ..response.usage
            },
            ..response
        };
        assert_eq!(
            render(&with_cost, "openrouter"),
            "openrouter jev-1.13.0: 0 answer(s), 328 in / 34 out, 0.000038 credits\n"
        );
    }

    #[test]
    fn a_wide_distribution_folds_its_tail() {
        let probabilities: BTreeMap<String, f64> =
            (0..20).map(|i| (format!("opt{i:02}"), 0.05)).collect();
        let response = response(BTreeMap::from([(
            "q".to_owned(),
            Answer::Choice {
                choice: "opt00".into(),
                confidence: 0.1,
                probabilities,
            },
        )]));
        let out = render(&response, "jev");
        assert!(out.contains("opt11 0.05, 8 more]"), "{out}");
        assert!(!out.contains("opt12"), "{out}");
    }

    #[test]
    fn ten_levels_sort_numerically() {
        let probabilities: BTreeMap<String, f64> = (0..=10).map(|i| (i.to_string(), 0.1)).collect();
        let line = render_answer(&Answer::Score {
            score: 5.5,
            confidence: 0.2,
            legend: BTreeMap::new(),
            probabilities,
        });
        assert!(
            line.starts_with("5.50 on 0-10 [0 0.10, 1 0.10, 2 0.10"),
            "{line}"
        );
        assert!(line.contains("9 0.10, 10 0.10"), "{line}");
    }

    #[test]
    fn no_answers_is_a_header_only() {
        let out = render(&response(BTreeMap::new()), "jev");
        assert_eq!(out, "jev jev-1.13.0: 0 answer(s), 328 in / 34 out\n");
    }

    #[test]
    fn past_the_cap_answers_drop_with_a_notice() {
        let filler = "x".repeat(2000);
        let answers: BTreeMap<String, Answer> = (0..40)
            .map(|i| {
                (
                    format!("q{i}"),
                    Answer::Choice {
                        choice: filler.clone(),
                        confidence: 1.0,
                        probabilities: BTreeMap::from([(filler.clone(), 1.0)]),
                    },
                )
            })
            .collect();
        let out = render(&response(answers), "jev");
        assert!(out.len() <= MAX_RENDER_BYTES + 4000, "{}", out.len());
        assert!(
            out.contains("more answer(s) dropped: the 8192 byte render cap"),
            "{out}"
        );
    }

    #[test]
    fn the_json_rendering_is_the_wire_shape() {
        let response = response(BTreeMap::from([(
            "q".to_owned(),
            Answer::Noul { noul: 0.5 },
        )]));
        let json = render_json(&response);
        let back: DecisionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, response);
        assert!(json.contains("\"input_tokens\": 328"), "{json}");
    }
}

#[cfg(test)]
mod oversized_answer_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// One answer longer than the whole cap used to compute
    /// `answers.len() - out.lines().count()`, which is `1 - 2`: an
    /// underflow panic in debug, a wrapped count in release.
    #[test]
    fn a_lone_oversized_answer_is_counted_as_dropped() {
        let question = "q".repeat(MAX_RENDER_BYTES);
        let response = DecisionResponse {
            model: "jev-1.13.0".into(),
            answers: BTreeMap::from([(question, Answer::Noul { noul: 0.5 })]),
            usage: crate::response::Usage {
                input_tokens: 1,
                cost: None,
                output_tokens: 1,
            },
        };

        let out = render(&response, "jev");

        assert!(
            out.ends_with("[1 more answer(s) dropped: the 8192 byte render cap]\n"),
            "{out}"
        );
    }

    /// The drop count is the answers that did not fit, whichever line
    /// hit the cap.
    #[test]
    fn only_the_answers_that_did_not_fit_are_dropped() {
        let mut answers = BTreeMap::new();
        answers.insert("short".to_owned(), Answer::Noul { noul: 0.5 });
        // `z` sorts after `short`, so the short line renders first and
        // the oversized one is the line that breaks the loop.
        answers.insert("z".repeat(MAX_RENDER_BYTES), Answer::Noul { noul: 0.5 });
        let response = DecisionResponse {
            model: "jev-1.13.0".into(),
            answers,
            usage: crate::response::Usage {
                input_tokens: 1,
                cost: None,
                output_tokens: 1,
            },
        };

        let out = render(&response, "jev");

        assert!(out.contains("\nshort: 0.50 uncertain\n"), "{out}");
        assert!(
            out.ends_with("[1 more answer(s) dropped: the 8192 byte render cap]\n"),
            "{out}"
        );
    }
}
