use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub title: String,
    pub category: String,
    pub prompt: String,
    pub answer: String,
}

pub fn builtins() -> Vec<Question> {
    [
        ("candy", "糖果最坏情形", "组合", "袋中有苹果味、桃子味、西瓜味糖果，各有圆形和五角星形。圆形的三种口味数量为7、9、8，五角星形为7、6、4。形状可通过触摸区分，口味无法通过触摸区分，允许按形状选择取出的糖果。须预先确定取多少颗圆形及多少颗五角星形，才能保证取出的糖果中有圆形苹果味配五角星桃子味，或圆形桃子味配五角星苹果味。最小总数是多少？只输出整数", "21"),
        ("pigeonhole", "抽屉原理", "组合", "从整数1到20中选取不同整数，至少选多少个才能保证其中两个数之和为21？只输出最小整数", "11"),
        ("crt", "同余约束", "数学", "求最小正整数n，使n除以3余2，除以5余3，除以7余2。只输出n", "23"),
        ("derangement", "错排计数", "组合", "四封不同的信随机放入四个对应的信封，每个信封恰好一封信。没有任何一封信放对信封的排列方式有多少种？只输出整数", "9"),
        ("bridge", "过桥规划", "约束推理", "四人过桥分别需要1、2、7、10分钟。最多两人一起过桥，共用一只手电，每次过桥必须带手电，双人按较慢者计时。所有人从同一岸出发，最少需要多少分钟全部过桥？只输出整数", "17"),
        ("liars", "真假陈述", "逻辑", "甲、乙、丙各说一句话：甲说乙在说谎，乙说丙在说谎，丙说甲和乙都在说谎。三句话中恰好一句为真，谁说了真话？只输出甲、乙或丙", "乙"),
        ("bayes", "贝叶斯概率", "概率", "某病患病率1%。检测灵敏度99%，未患病者假阳性率1%。随机选一人，检测呈阳性，其患病概率是多少？只输出最简分数，格式a/b", "1/2"),
        ("paths", "网格路径", "组合", "只能向右或向上移动，在整数格点从(0,0)走到(4,4)，不经过(2,2)的最短路径有多少条？只输出整数", "34"),
        ("subset", "子集约束", "数学", "集合{1,2,3,4,5,6}的子集中，元素和等于10的子集有多少个？只输出整数", "5"),
        ("code", "代码执行", "代码阅读", "执行下面Python代码后x的值是多少？只输出整数\nx=0\nfor i in range(1,6):\n    for j in range(i):\n        if (i+j)%2 == 0:\n            x += i-j", "16"),
    ].into_iter().map(|(id,title,category,prompt,answer)| Question {
        id:id.into(), title:title.into(), category:category.into(), prompt:prompt.into(), answer:answer.into()
    }).collect()
}

pub fn validate_custom(questions: &[Question]) -> Result<(), &'static str> {
    if questions.len() > 30 {
        return Err("自定义题目最多30道");
    }
    let mut ids = std::collections::BTreeSet::new();
    for q in questions {
        if !q.id.starts_with("custom_")
            || q.id.len() > 64
            || !q.id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !ids.insert(&q.id)
        {
            return Err("题目ID须以custom_开头且唯一，仅使用字母、数字和下划线");
        }
        for (text, max) in [
            (&q.title, 120),
            (&q.category, 80),
            (&q.prompt, 8000),
            (&q.answer, 256),
        ] {
            if text.trim().is_empty() || text.len() > max {
                return Err("题目字段为空或超过长度限制");
            }
        }
    }
    if serde_json::to_vec(questions)
        .map_err(|_| "题库编码失败")?
        .len()
        > 240_000
    {
        return Err("自定义题库超过240KB容量限制");
    }
    Ok(())
}

// 仅忽略首尾空白及包裹整个答案的 Markdown 标记，不从解释中猜测答案。
pub fn normalize(answer: &str) -> &str {
    let value = answer.trim();
    value
        .strip_prefix("**")
        .and_then(|v| v.strip_suffix("**"))
        .or_else(|| value.strip_prefix('`').and_then(|v| v.strip_suffix('`')))
        .unwrap_or(value)
        .trim()
}

pub fn grade(actual: &str, expected: &str) -> bool {
    !actual.trim().is_empty() && normalize(actual) == normalize(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_grading() {
        assert!(grade(" **21**\n", "21"));
        assert!(grade("`1/2`", "1/2"));
        assert!(!grade("答案是21，也可能20", "21"));
        assert!(!grade("", ""));
    }
    #[test]
    fn bank_is_unique_and_custom_cannot_override() {
        let bank = builtins();
        assert_eq!(bank.len(), 10);
        assert_eq!(
            bank.iter()
                .map(|q| &q.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            10
        );
        assert!(validate_custom(&[bank[0].clone()]).is_err());
    }
    #[test]
    fn candy_shape_strategy_requires_twenty_one() {
        let mut failing = [[false; 18]; 25];
        for ra in 0..=7 {
            for rp in 0..=9 {
                for rw in 0..=8 {
                    for sa in 0..=7 {
                        for sp in 0..=6 {
                            for sw in 0..=4 {
                                if !(ra > 0 && sp > 0 || rp > 0 && sa > 0) {
                                    failing[ra + rp + rw][sa + sp + sw] = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        let optimum = failing
            .iter()
            .enumerate()
            .flat_map(|(r, row)| {
                row.iter()
                    .enumerate()
                    .filter_map(move |(s, fails)| (!fails).then_some(r + s))
            })
            .min();
        assert_eq!(optimum, Some(21));
        assert!(!failing[9][12]);
    }

    #[test]
    fn verify_enumerable_answers() {
        let subsets = (0..64)
            .filter(|mask| (1..=6).filter(|i| mask & (1 << (i - 1)) != 0).sum::<u32>() == 10)
            .count();
        assert_eq!(subsets, 5);
        let mut x = 0;
        for i in 1..6 {
            for j in 0..i {
                if (i + j) % 2 == 0 {
                    x += i - j;
                }
            }
        }
        assert_eq!(x, 16);
        let truth: Vec<_> = (0..8)
            .filter(|mask| {
                let a = mask & 1 != 0;
                let b = mask & 2 != 0;
                let c = mask & 4 != 0;
                a == !b
                    && b == !c
                    && c == (!a && !b)
                    && u8::from(a) + u8::from(b) + u8::from(c) == 1
            })
            .collect();
        assert_eq!(truth, vec![2]);
        assert_eq!(
            (1..1000).find(|n| n % 3 == 2 && n % 5 == 3 && n % 7 == 2),
            Some(23)
        );
        let mut paths = [[0_u32; 5]; 5];
        paths[0][0] = 1;
        for x in 0..5 {
            for y in 0..5 {
                if (x, y) == (0, 0) || (x, y) == (2, 2) {
                    continue;
                }
                paths[x][y] = if x > 0 { paths[x - 1][y] } else { 0 }
                    + if y > 0 { paths[x][y - 1] } else { 0 };
            }
        }
        assert_eq!(paths[4][4], 34);
        let mut derangements = 0;
        for a in 0..4 {
            for b in 0..4 {
                for c in 0..4 {
                    for d in 0..4 {
                        if [a, b, c, d]
                            .into_iter()
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                            == 4
                            && a != 0
                            && b != 1
                            && c != 2
                            && d != 3
                        {
                            derangements += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(derangements, 9);
        let mut distances = [u32::MAX; 32];
        distances[0] = 0;
        let times = [1, 2, 7, 10];
        for _ in 0..32 {
            for state in 0..32 {
                if distances[state] == u32::MAX {
                    continue;
                }
                let side = (state >> 4) & 1;
                for first in 0..4 {
                    for second in first..4 {
                        if ((state >> first) & 1) != side || ((state >> second) & 1) != side {
                            continue;
                        }
                        let next = state
                            ^ (1 << first)
                            ^ if first != second { 1 << second } else { 0 }
                            ^ 16;
                        distances[next] =
                            distances[next].min(distances[state] + times[first].max(times[second]));
                    }
                }
            }
        }
        assert_eq!(distances[31], 17);
    }
}
