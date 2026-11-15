//! CLI：`keel-ingest <input.csv> <output.csv> [--snap]`
//!
//! 读真实源日线 CSV → 黄金 CSV；清洗报告（处置/缺口/计数）写 stderr。
//! 退出码：0 成功；1 清洗失败（表头/空集/用法）；2 IO 错误。

use keel_ingest::{clean_daily_csv, RowAction, SnapMode};
use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut snap = false;
    for a in &args {
        if a == "--snap" {
            snap = true;
        } else {
            positional.push(a.clone());
        }
    }
    if positional.len() != 2 {
        eprintln!("用法: keel-ingest <input.csv> <output.csv> [--snap]");
        return ExitCode::from(1);
    }
    let text = match fs::read_to_string(&positional[0]) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读取失败 {}: {e}", positional[0]);
            return ExitCode::from(2);
        }
    };
    let mode = if snap { SnapMode::Snap } else { SnapMode::Strict };
    match clean_daily_csv(&text, mode) {
        Ok((golden, rep)) => {
            if let Err(e) = fs::write(&positional[1], &golden) {
                eprintln!("写入失败 {}: {e}", positional[1]);
                return ExitCode::from(2);
            }
            eprintln!(
                "清洗完成：ok={} snap={} skipped={} gaps={}",
                rep.ok_rows, rep.snap_rows, rep.skipped_rows, rep.gaps.len()
            );
            for (line_no, action) in &rep.actions {
                match action {
                    RowAction::Ok => {}
                    RowAction::Snap { field, from, to } => {
                        eprintln!("  第 {line_no} 行 SNAP {field}: {from} -> {to}");
                    }
                    RowAction::Skip { reason } => {
                        eprintln!("  第 {line_no} 行 SKIP: {reason}");
                    }
                }
            }
            for g in &rep.gaps {
                eprintln!("  日期缺口: {} -> {}", g.from, g.to);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("清洗失败：{e}");
            ExitCode::from(1)
        }
    }
}
