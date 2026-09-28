//! 检索质量评测运行器：跑一份固定抽样问题集，输出带出处的通过率与 MRR
//! （roadmap v0.3「检索质量评测常态化」；评测核心见 `agent::eval`）。
//!
//! 用法：cargo run --release --example eval_retrieval -- <问题集.json> [选项]
//!
//! 选项：
//!   --root <目录>      覆盖 local 语料根目录（语料在别的机器/路径时用）；
//!                      repo / mediawiki 源不受影响
//!   --llm <config.json> 启用自然语言用例（question 字段）：经 LLM 翻译成英文
//!                      关键词后双语检索（与技能层同款流程）。config 取
//!                      应用 config.json 的 agent.llm 段，或 {"llm":{...}}，
//!                      或直接就是 llm 对象
//!   --min-rate <f>     通过率低于该值时退出码 1（默认 1.0，用于常态化门禁）
//!
//! 问题集格式见 `rust/eval/*.json`：`corpus` 段与 `agent.skills[].sources[]`
//! 完全同构（type: local / mediawiki / repo）。

use chatroom_bridge::agent::build_source;
use chatroom_bridge::agent::eval::{QuestionSet, run_question_set};
use chatroom_bridge::agent::llm::LlmClient;
use chatroom_bridge::config::{LlmConfig, SourceConfig};

fn describe_corpus(corpus: &SourceConfig) -> String {
    match corpus {
        SourceConfig::Local { root, extensions, .. } => {
            format!("local {root}（extensions: {extensions:?}）")
        }
        SourceConfig::Repo { repo, branch, subdir, .. } => {
            let subdir = if subdir.is_empty() { "" } else { subdir.as_str() };
            format!("repo {repo}@{branch}（subdir: {subdir}）")
        }
        SourceConfig::Mediawiki { api_url, .. } => format!("mediawiki {api_url}"),
    }
}

/// 从配置文件提取 LLM 配置：认三种形态——应用 config.json（agent.llm）、
/// {"llm":{...}}、裸 LlmConfig 文件。
fn load_llm(path: &str) -> LlmClient {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|err| die(&format!("读取 LLM 配置 {path} 失败: {err}")));
    let value: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|err| die(&format!("LLM 配置 {path} 不是合法 JSON: {err}")));
    let llm_value = if value
        .get("agent")
        .and_then(|agent| agent.get("llm"))
        .map_or(false, |llm| llm.is_object())
    {
        value["agent"]["llm"].clone()
    } else if value.get("llm").map_or(false, |llm| llm.is_object()) {
        value["llm"].clone()
    } else if value.get("api_url").map_or(false, |v| v.is_string()) {
        value.clone()
    } else {
        die(&format!("配置 {path} 里找不到 LLM 配置（agent.llm / llm / 裸 api_url）"));
    };
    let cfg: LlmConfig = serde_json::from_value(llm_value)
        .unwrap_or_else(|err| die(&format!("LLM 配置 {path} 解析失败: {err}")));
    if cfg.api_url.is_empty() {
        die(&format!("LLM 配置 {path} 的 api_url 为空"));
    }
    LlmClient::new(cfg).unwrap_or_else(|err| die(&format!("LLM 客户端构建失败: {err}")))
}

fn die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut json_path: Option<String> = None;
    let mut root_override: Option<String> = None;
    let mut llm_config_path: Option<String> = None;
    let mut min_rate = 1.0f64;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                root_override = Some(args.next().unwrap_or_else(|| die("--root 需要一个目录参数")))
            }
            "--llm" => {
                llm_config_path =
                    Some(args.next().unwrap_or_else(|| die("--llm 需要一个配置文件参数")))
            }
            "--min-rate" => {
                let raw = args.next().unwrap_or_else(|| die("--min-rate 需要一个数值参数"));
                min_rate = raw.parse().unwrap_or_else(|_| die("--min-rate 不是合法数值"));
            }
            other => {
                if json_path.is_none() {
                    json_path = Some(other.to_string());
                } else {
                    die(&format!("未知参数：{other}"));
                }
            }
        }
    }
    let Some(json_path) = json_path else {
        die("用法: eval_retrieval <问题集.json> [--root <本地语料目录>] [--llm <config.json>] [--min-rate 1.0]");
    };

    let raw = std::fs::read_to_string(&json_path)
        .unwrap_or_else(|err| die(&format!("读取问题集 {json_path} 失败: {err}")));
    let set: QuestionSet = serde_json::from_str(&raw)
        .unwrap_or_else(|err| die(&format!("问题集 {json_path} 解析失败: {err}")));
    if set.cases.is_empty() {
        die(&format!("问题集 {json_path} 没有用例（cases 为空）"));
    }

    let Some(mut corpus) = set.corpus.clone() else {
        die(&format!("问题集 {json_path} 缺少 corpus 段"));
    };
    if let Some(root) = &root_override {
        match &mut corpus {
            SourceConfig::Local { root: slot, .. } => *slot = root.clone(),
            _ => eprintln!("警告: --root 只对 local 语料生效，当前语料类型不匹配，忽略"),
        }
    }

    let llm = llm_config_path.as_deref().map(load_llm);
    let skipped_count = set.cases.iter().filter(|c| !c.question.is_empty()).count();
    if skipped_count > 0 && llm.is_none() {
        eprintln!(
            "提示: 问题集含 {skipped_count} 道自然语言用例（question），未配 --llm，将跳过（不计入通过率）"
        );
    }

    let source = build_source(&corpus).unwrap_or_else(|err| die(&format!("语料无效: {err}")));
    println!(
        "问题集「{}」{}：{} 题，top_k={}，语料 {}",
        set.name,
        if set.description.is_empty() { "" } else { set.description.as_str() },
        set.cases.len(),
        set.top_k,
        describe_corpus(&corpus),
    );

    let report = run_question_set(&set, source.as_ref(), llm.as_ref()).await;
    let id_width = report.results.iter().map(|r| r.id.chars().count()).max().unwrap_or(0);
    for (idx, result) in report.results.iter().enumerate() {
        let status = if result.skipped {
            "SKIP"
        } else if result.pass {
            "PASS"
        } else {
            "FAIL"
        };
        let rank_note = if result.skipped {
            "需 --llm".to_string()
        } else {
            match result.rank {
                Some(rank) => format!("命中 rank {rank}"),
                None => "未命中".to_string(),
            }
        };
        println!(
            "{:>2}. [{status}] {:<id_width$} 「{}」{rank_note}（≤{}）",
            idx + 1,
            result.id,
            result.query,
            result.max_rank,
        );
        if result.used_translation {
            println!("      检索词: {:?}（含 LLM 译文）", result.queries);
        }
        let show = if result.pass { 1 } else { report.top_k };
        for (rank, title, locator) in result.hits.iter().take(show) {
            println!("      {rank}. {title}  [{locator}]");
        }
        if !result.note.is_empty() && !result.pass && !result.skipped {
            println!("      备注: {}", result.note);
        }
    }

    println!(
        "\n汇总：{}/{} 通过（{:.0}%）· MRR {:.3} · 耗时 {:.1?}",
        report.pass_count(),
        report.results.iter().filter(|r| !r.skipped).count(),
        report.pass_rate() * 100.0,
        report.mrr(),
        report.elapsed,
    );
    if report.pass_rate() < min_rate {
        eprintln!(
            "未达门禁阈值 {min_rate:.2}（实际 {:.2}），退出码 1",
            report.pass_rate()
        );
        std::process::exit(1);
    }
}
