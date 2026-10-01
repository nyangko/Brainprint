use brainprint_engine::diagnostics::{MAX_DIAGNOSTICS, MAX_MESSAGE_BYTES};

use super::*;

const SEVERITIES: [Severity; 5] = [
    Severity::Unknown,
    Severity::Help,
    Severity::Note,
    Severity::Warning,
    Severity::Error,
];

fn item(severity: Severity, command: usize, index: usize, big: bool) -> Diagnostic {
    let fill = |tag: &str| {
        let text = format!("{tag}-{command}-{index}-");
        if big {
            format!("{text}{}", "x".repeat(MAX_MESSAGE_BYTES - text.len()))
        } else {
            text
        }
    };
    Diagnostic {
        severity,
        code: Some(fill("code")),
        message: fill("message"),
        path: DiagnosticPath::Workspace(fill("path")),
        line: Some(1),
        column: Some(2),
        stream: Stream::Stderr,
        message_truncated: big,
    }
}

fn captured(items: Vec<Diagnostic>, omitted: u64) -> Kept {
    let kept = items.len() as u64;
    Kept::Captured {
        stream_status: StreamStatusWire::Complete,
        diagnostics: Some(DiagnosticSummary {
            items,
            observed: kept + 3 + omitted,
            deduplicated: 3,
            omitted,
            parse_misses: 7,
        }),
        raw: RawAvailabilityWire::NotRequested,
    }
}

fn summaries(captures: &[CommandCaptureWire]) -> Vec<&DiagnosticSummaryWire> {
    captures
        .iter()
        .filter_map(|capture| match capture {
            CommandCaptureWire::Captured {
                diagnostics: Some(summary),
                ..
            } => Some(summary),
            _ => None,
        })
        .collect()
}

#[test]
fn small_diagnostics_all_go_out_in_their_own_order() {
    let kept = vec![
        Kept::NotRequested,
        captured(
            vec![
                item(Severity::Error, 1, 0, false),
                item(Severity::Note, 1, 1, false),
            ],
            0,
        ),
        Kept::NotRun,
    ];
    let shaped = shape(kept);
    assert_eq!(shaped[0], CommandCaptureWire::NotRequested);
    assert_eq!(shaped[2], CommandCaptureWire::NotRun);
    let summary = summaries(&shaped)[0];
    assert_eq!(summary.items.len(), 2);
    assert_eq!(summary.items[0].message, "message-1-0-");
    assert_eq!(
        summary.items[0].path,
        DiagnosticPathWire::Workspace {
            path: "path-1-0-".to_owned()
        }
    );
    assert_eq!(
        (
            summary.observed,
            summary.deduplicated,
            summary.omitted,
            summary.parse_misses,
            summary.delivery_omitted
        ),
        (5, 3, 0, 7, 0)
    );
}

/// 16 commands × 64 maximal diagnostics: far over the budget. What goes
/// out is a prefix of (severity, command, item) order within 512 KiB;
/// the rest is `delivery_omitted`, never `omitted`.
#[test]
fn the_budget_keeps_the_highest_priority_prefix() {
    let commands = 16;
    let kept: Vec<Kept> = (0..commands)
        .map(|command| {
            let items = (0..MAX_DIAGNOSTICS)
                .map(|index| item(SEVERITIES[(command + index) % 5], command, index, true))
                .collect();
            captured(items, 9)
        })
        .collect();
    let shaped = shape(kept);
    let summaries = summaries(&shaped);
    assert_eq!(summaries.len(), commands);

    let mut sent = 0;
    let mut chosen = Vec::new();
    for (command, summary) in summaries.iter().enumerate() {
        assert_eq!(summary.omitted, 9, "the engine's count is unchanged");
        assert_eq!(summary.parse_misses, 7);
        assert_eq!(
            summary.items.len() as u64 + summary.delivery_omitted,
            MAX_DIAGNOSTICS as u64
        );
        for item in &summary.items {
            sent += serde_json::to_vec(item).expect("json").len();
            let index: usize = item.message["message-".len()..]
                .split('-')
                .nth(1)
                .expect("index")
                .parse()
                .expect("number");
            chosen.push((SEVERITIES[(command + index) % 5], command, index));
        }
        // Within a command, the engine's order.
        let indices: Vec<_> = summary
            .items
            .iter()
            .map(|item| item.message.clone())
            .collect();
        let mut sorted = indices.clone();
        sorted.sort_by_key(|message| {
            message["message-".len()..]
                .split('-')
                .nth(1)
                .and_then(|index| index.parse::<usize>().ok())
        });
        assert_eq!(indices, sorted);
    }
    assert!(sent <= MAX_DIAGNOSTIC_WIRE_BYTES, "{sent}");
    let delivered: u64 = summaries.iter().map(|s| s.items.len() as u64).sum();
    let left: u64 = summaries.iter().map(|s| s.delivery_omitted).sum();
    assert!(delivered > 0 && left > 0);
    assert_eq!(delivered + left, (commands * MAX_DIAGNOSTICS) as u64);

    // A prefix: every chosen item precedes every left-out one.
    chosen.sort_unstable();
    let mut all: Vec<_> = (0..commands)
        .flat_map(|command| {
            (0..MAX_DIAGNOSTICS)
                .map(move |index| (SEVERITIES[(command + index) % 5], command, index))
        })
        .collect();
    all.sort_unstable();
    assert_eq!(chosen, all[..chosen.len()]);
    assert!(
        chosen
            .iter()
            .all(|(severity, ..)| *severity == Severity::Error)
    );
}
