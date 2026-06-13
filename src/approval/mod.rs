use crate::models::{FixAction, Problem};

pub enum Decision {
    Approved,
    Rejected,
    Skipped,
}

pub async fn prompt(problem: &Problem, action: &FixAction) -> Decision {
    println!();
    println!("┌─ PROPOSED FIX ──────────────────────────────────────────────────┐");
    println!("│ Diagnosis  : {}", problem.diagnosis);
    println!("│ Root cause : {}", problem.root_cause);
    println!("│ Confidence : {:.0}%", problem.confidence * 100.0);
    println!("│ Action     : {action:?}");
    println!("│ Reasoning  : {}", problem.reasoning);
    println!("│ Side fx    : {}", problem.side_effects);
    println!("│ Undo       : {}", problem.undo_instructions);
    println!("└─────────────────────────────────────────────────────────────────┘");
    println!("Execute? [y]es / [n]o / [s]kip  > ");

    let answer = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        line.trim().to_lowercase()
    })
    .await
    .unwrap_or_default();

    match answer.as_str() {
        "y" | "yes" => Decision::Approved,
        "n" | "no" => Decision::Rejected,
        _ => Decision::Skipped,
    }
}
