//! 端到端往返测试：建 blob → 加载 → 查询。
//!
//! 用**内存里的小词典**，不依赖 25 MB 的真实数据文件 ——
//! 这样 `cargo test` 在任何环境都能跑。

use pinyin_engine::{build_dict, Engine};

/// 一份够用的小词典，覆盖多切分、单字回退、词频差异等情形
const SMALL: &[(&str, &str, u32)] = &[
    // 单字（模拟「单字词频远高于词」的真实情况）
    ("你", "ni", 3_300_000),
    ("好", "hao", 2_800_000),
    ("是", "shi", 33_000_000),
    ("世", "shi", 900_000),
    ("界", "jie", 700_000),
    ("先", "xian", 1_200_000),
    ("西", "xi", 800_000),
    ("安", "an", 900_000),
    ("我", "wo", 5_000_000),
    ("想", "xiang", 1_500_000),
    ("中", "zhong", 2_000_000),
    ("国", "guo", 1_800_000),
    ("人", "ren", 2_500_000),
    // 词
    ("你好", "ni hao", 480_000),
    ("世界", "shi jie", 480_000),
    ("西安", "xi an", 90_000),
    ("我想", "wo xiang", 300_000),
    ("中国", "zhong guo", 900_000),
    ("中国人", "zhong guo ren", 200_000),
    ("你好世界", "ni hao shi jie", 5_000),
    ("拟好", "ni hao", 3_000),
];

fn engine() -> Engine {
    let (bytes, _) = build_dict(SMALL.iter().copied()).expect("建表失败");
    Engine::from_bytes(&bytes).expect("加载失败")
}

#[test]
fn roundtrip_basic() {
    let e = engine();
    assert_eq!(e.dict().n_entries(), SMALL.len(), "条目数应等于输入数（无重复时）");
    assert_eq!(e.syllable_count(), 12, "音节数");
}

#[test]
fn top1_is_the_expected_word() {
    let e = engine();
    let cases = [
        ("nihao", "你好"),
        ("shijie", "世界"),
        ("zhongguo", "中国"),
        ("woxiang", "我想"),
        ("zhongguoren", "中国人"),
    ];
    for (input, want) in cases {
        let c = e.candidates(input, 9);
        assert!(!c.is_empty(), "{input} 应有候选");
        assert_eq!(
            c[0].word,
            want,
            "{input} 的 top-1 应为 {want}，实际 {:?}",
            c.iter().map(|x| &x.word).collect::<Vec<_>>()
        );
    }
}

/// **本引擎只做前缀匹配** —— 这一条把这个限制钉住，免得日后误以为是 bug。
///
/// 输入 `woshi` 的分词是 `[wo, shi]`，遍历只走 `wo` → `wo+shi` 这条路，
/// 因此只能出「我」（覆盖前缀 `wo`），**出不了单字「是」**（它不在前缀上）。
///
/// 要覆盖整句（`woshi` → `我` + `是`）需要**词图 + 整句解码**，
/// 那是 MVP 之后的事（M4 验收定为「多个词组」而非「整句」）。
#[test]
fn prefix_only_limitation_is_documented() {
    let e = engine();
    let c = e.candidates("woshi", 9);
    let words: Vec<&str> = c.iter().map(|x| x.word.as_str()).collect();
    assert_eq!(words[0], "我", "只应出覆盖前缀的「我」，实际 {words:?}");
    assert!(
        !words.contains(&"是"),
        "单字「是」不在前缀上，本引擎不应产出它（整句解码才需要）"
    );
}

/// 单字词频比词高两个数量级时，完整覆盖的词仍须胜出。
/// 这是实测发现的真问题（`shijie` 曾把「是」排在「世界」前）。
#[test]
fn complete_word_beats_high_freq_single_char() {
    let e = engine();
    let c = e.candidates("shijie", 9);
    let words: Vec<&str> = c.iter().map(|x| x.word.as_str()).collect();
    let pos_world = words.iter().position(|w| *w == "世界").expect("世界应在候选里");
    let pos_shi = words.iter().position(|w| *w == "是");
    if let Some(p) = pos_shi {
        assert!(pos_world < p, "「世界」应排在「是」之前，实际 {words:?}");
    }
}

/// `xian` 的两种切分都要能出词 —— 这是 §6.2 明确要求的行为。
#[test]
fn ambiguous_input_exposes_both_readings() {
    let e = engine();
    let c = e.candidates("xian", 9);
    let words: Vec<&str> = c.iter().map(|x| x.word.as_str()).collect();
    assert!(words.contains(&"先"), "缺 [xian] 读法: {words:?}");
    assert!(words.contains(&"西安"), "缺 [xi,an] 读法: {words:?}");
}

/// 没有完整匹配时，覆盖多的候选优先（`nihaoshijie` 场景）。
#[test]
fn partial_match_ranks_by_coverage() {
    let e = engine();
    // 词典里有「你好世界」，先确认它能被查到
    let full = e.candidates("nihaoshijie", 9);
    assert_eq!(full[0].word, "你好世界", "有完整匹配时应优先");

    // 把长词去掉再查，应退化为按覆盖度排（「你好」优于「你」）
    let short: Vec<(&str, &str, u32)> = SMALL
        .iter()
        .copied()
        .filter(|(w, _, _)| *w != "你好世界")
        .collect();
    let (bytes, _) = build_dict(short).unwrap();
    let e2 = Engine::from_bytes(&bytes).unwrap();
    let c = e2.candidates("nihaoshijie", 9);
    assert_eq!(c[0].word, "你好", "无完整匹配时「你好」(2/4) 应优于单字 (1/4)");
}

#[test]
fn garbage_input_yields_nothing() {
    let e = engine();
    assert!(e.candidates("", 9).is_empty());
    assert!(e.candidates("zzzz", 9).is_empty());
    assert!(e.candidates("NIHAO", 9).is_empty(), "大写不应被接受");
}

/// 超长输入不能卡死，也不能返回空（应降级为最长切分）。
#[test]
fn overlong_input_degrades_gracefully() {
    let e = engine();
    let long = "nihao".repeat(10); // 50 字符 > MAX_INPUT_LEN(32)
    let c = e.candidates(&long, 9);
    assert!(!c.is_empty(), "超长输入应降级处理而不是返回空");
}

/// 词典损坏时给出明确错误，而不是 panic 或读出垃圾。
#[test]
fn corrupt_dict_is_rejected() {
    use pinyin_engine::DictError;
    assert!(matches!(Engine::from_bytes(b"too short"), Err(DictError::Truncated)));

    let (mut bytes, _) = build_dict(SMALL.iter().copied()).unwrap();
    bytes[0] = b'X';
    assert!(matches!(Engine::from_bytes(&bytes), Err(DictError::BadMagic)));

    let (mut bytes, _) = build_dict(SMALL.iter().copied()).unwrap();
    bytes[8] = 99;
    assert!(matches!(Engine::from_bytes(&bytes), Err(DictError::BadVersion(99))));
}

/// 建表结果必须可复现：同样的输入两次建出同样的字节。
#[test]
fn build_is_deterministic() {
    let (a, _) = build_dict(SMALL.iter().copied()).unwrap();
    let (b, _) = build_dict(SMALL.iter().copied()).unwrap();
    assert_eq!(a, b, "同样输入应产生逐字节相同的 blob");
}

/// 非法音节被跳过而不是让整次构建失败（真实词库总有脏数据）。
#[test]
fn dirty_syllables_are_skipped() {
    let (bytes, stats) = build_dict([
        ("好", "hao", 100),
        ("坏", "huai3", 100), // 带声调数字，非法
        ("糟", "ZZZ", 100),   // 大写，非法
        ("了", "le", 100),
    ])
    .unwrap();
    assert_eq!(stats.entries, 2, "应只保留 2 条合法条目");
    let e = Engine::from_bytes(&bytes).unwrap();
    assert!(!e.candidates("hao", 9).is_empty());
    assert!(e.candidates("huai3", 9).is_empty());
}

// ─────────── 末尾音节前缀展开（「从第一个字母就出候选」）───────────

/// 单个字母就该出候选 —— 用户打 `s` 时想看到「是/三/说」，
/// 而 `s` 本身不是完整音节，必须按音节前缀展开。
#[test]
fn single_letter_yields_candidates() {
    let e = engine();
    for (input, must_contain) in [("s", "是"), ("n", "你"), ("w", "我"), ("x", "先")] {
        let c = e.candidates(input, 9);
        let words: Vec<&str> = c.iter().map(|x| x.word.as_str()).collect();
        assert!(!words.is_empty(), "{input} 应有候选，实际为空");
        assert!(
            words.contains(&must_contain),
            "{input} 的候选里应含 {must_contain}，实际 {words:?}"
        );
    }
}

/// 不完整的音节前缀也要出词：`nih` 应能出「你好」（`h` 展开成 `hao`）。
#[test]
fn partial_syllable_prefix_expands() {
    let e = engine();
    let c = e.candidates("nih", 9);
    let words: Vec<&str> = c.iter().map(|x| x.word.as_str()).collect();
    assert!(
        words.contains(&"你好"),
        "`nih` 应能展开出「你好」，实际 {words:?}"
    );
}

/// 展开时「已完整部分」自身**不能**被当成完整覆盖 ——
/// 否则 `nih` 的「你」会靠 complete 奖励压过「你好」。
/// （这是实测踩到的坑：collect 原先按 ids.len() 算 total。）
#[test]
fn partial_expansion_scores_consistently() {
    let e = engine();
    let c = e.candidates("nih", 9);
    let words: Vec<&str> = c.iter().map(|x| x.word.as_str()).collect();
    let pos_ni = words.iter().position(|w| *w == "你");
    let pos_nihao = words.iter().position(|w| *w == "你好").expect("你好 应在候选里");
    if let Some(p) = pos_ni {
        assert!(
            pos_nihao < p,
            "完整覆盖的「你好」应排在只覆盖前缀的「你」之前，实际 {words:?}"
        );
    }
}

/// 完全无法匹配的串仍应返回空（展开也不能凭空造词）。
#[test]
fn unmatched_prefix_still_empty() {
    let e = engine();
    assert!(e.candidates("zzzz", 9).is_empty(), "zzzz 不该有候选");
    assert!(e.candidates("qqq", 9).is_empty(), "qqq 不该有候选");
}
