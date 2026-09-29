// findText 检索(content-find.ts 移植;原 content/mod.rs 的正文)
use std::sync::LazyLock;

use regex::Regex;

pub const CONTEXT_CHARS: usize = 400;
pub const MAX_OUTPUT_CHARS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindMode {
    Exact,
    CaseInsensitive,
    Fuzzy,
}

impl FindMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exact" => Some(Self::Exact),
            "case-insensitive" => Some(Self::CaseInsensitive),
            "fuzzy" => Some(Self::Fuzzy),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::CaseInsensitive => "case-insensitive",
            Self::Fuzzy => "fuzzy",
        }
    }
}

#[derive(Debug, Clone)]
struct Match {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy)]
struct Range {
    start: usize,
    end: usize,
}

/// 归一化:NFD 分解 → 剥变音符 → 小写(fuzzy 的 token 比较)。
pub fn normalize(value: &str) -> String {
    static DIACRITICS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\p{M}").expect("static regex"));
    use unicode_normalization::UnicodeNormalization;
    let decomposed: String = value.nfkd().collect();
    let stripped = DIACRITICS.replace_all(&decomposed, "");
    stripped.to_lowercase()
}

/// 编辑距离 ≤ maximum(带行内最小值提前退出,content-find.ts)。
fn edit_distance_within(left: &[char], right: &[char], maximum: usize) -> bool {
    if left.len().abs_diff(right.len()) > maximum {
        return false;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, left_char) in left.iter().enumerate() {
        let mut current = vec![i + 1];
        let mut row_minimum = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let value = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(left_char != right_char));
            current.push(value);
            row_minimum = row_minimum.min(value);
        }
        if row_minimum > maximum {
            return false;
        }
        previous = current;
    }
    previous[right.len()] <= maximum
}

fn fuzzy_threshold(query_token_len: usize) -> usize {
    if query_token_len >= 9 {
        2
    } else if query_token_len >= 5 {
        1
    } else {
        0
    }
}

/// 字符级文本视图(索引即 char 下标)。
struct CharText {
    chars: Vec<char>,
}

impl CharText {
    fn new(text: &str) -> Self {
        CharText {
            chars: text.chars().collect(),
        }
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.chars
            .get(start..end)
            .map(|chars| chars.iter().collect())
            .unwrap_or_default()
    }

    /// \s+ 连续空白区段。
    fn whitespace_runs(&self) -> Vec<Range> {
        let mut runs = Vec::new();
        let mut index = 0;
        while index < self.chars.len() {
            if self.chars[index].is_whitespace() {
                let start = index;
                while index < self.chars.len() && self.chars[index].is_whitespace() {
                    index += 1;
                }
                runs.push(Range { start, end: index });
            } else {
                index += 1;
            }
        }
        runs
    }

    /// token([\p{L}\p{N}]+)及其 char 区段。
    fn tokens(&self, start: usize, end: usize) -> Vec<(usize, usize)> {
        let mut tokens = Vec::new();
        let mut index = start;
        while index < end {
            let c = self.chars[index];
            if c.is_alphabetic() || c.is_numeric() {
                let token_start = index;
                while index < end {
                    let c = self.chars[index];
                    if c.is_alphabetic() || c.is_numeric() {
                        index += 1;
                    } else {
                        break;
                    }
                }
                tokens.push((token_start, index));
            } else {
                index += 1;
            }
        }
        tokens
    }
}

fn literal_matches(text: &CharText, query: &str, case_insensitive: bool) -> Vec<Match> {
    let needle: String = if case_insensitive {
        query.to_lowercase()
    } else {
        query.to_string()
    };
    if needle.is_empty() {
        return Vec::new();
    }
    // 折叠后的 haystack + 每个折叠字符到原始 char 下标的映射
    let mut folded: Vec<char> = Vec::with_capacity(text.len());
    let mut origin: Vec<usize> = Vec::with_capacity(text.len());
    for (index, c) in text.chars.iter().enumerate() {
        if case_insensitive {
            for lower in c.to_lowercase() {
                folded.push(lower);
                origin.push(index);
            }
        } else {
            folded.push(*c);
            origin.push(index);
        }
    }
    let haystack: String = folded.iter().collect();
    let mut matches = Vec::new();
    let mut byte_position = 0usize;
    while let Some(found) = haystack[byte_position..].find(&needle) {
        let found_byte = byte_position + found;
        let folded_index = haystack[..found_byte].chars().count();
        let last = folded_index + needle.chars().count() - 1;
        matches.push(Match {
            start: origin[folded_index],
            end: origin[last] + 1,
        });
        // 非重叠推进(TS:max(needle.length, 1))
        byte_position = found_byte + needle.len();
    }
    matches
}

/// 段落(非空行 + 单换行连接;空行分段)。
fn paragraph_ranges(text: &CharText) -> Vec<Range> {
    let mut paragraphs = Vec::new();
    let mut current_start: Option<usize> = None;
    let mut index = 0usize;
    while index < text.len() {
        let line_end = text.chars[index..]
            .iter()
            .position(|c| *c == '\n')
            .map(|offset| index + offset)
            .unwrap_or(text.len());
        let line_is_empty = text.slice(index, line_end).trim().is_empty();
        if !line_is_empty && current_start.is_none() {
            current_start = Some(index);
        }
        if line_is_empty || line_end == text.len() {
            if let Some(start) = current_start.take() {
                if start < line_end {
                    paragraphs.push(Range {
                        start,
                        end: line_end,
                    });
                }
            }
        }
        index = line_end + 1;
    }
    paragraphs
}

fn fuzzy_matches(text: &CharText, query: &str) -> Vec<Match> {
    let normalized_query = normalize(query);
    let query_tokens: Vec<Vec<char>> = token_strings(&normalized_query)
        .into_iter()
        .map(|token| token.chars().collect())
        .collect();
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for paragraph in paragraph_ranges(text) {
        if text.slice(paragraph.start, paragraph.end).trim().is_empty() {
            continue;
        }
        let tokens = text.tokens(paragraph.start, paragraph.end);
        let mut matched_count = 0usize;
        for query_token in &query_tokens {
            let threshold = fuzzy_threshold(query_token.len());
            let hit = tokens.iter().any(|(start, end)| {
                let candidate: Vec<char> = normalize(&text.slice(*start, *end)).chars().collect();
                edit_distance_within(query_token, &candidate, threshold)
            });
            if hit {
                matched_count += 1;
            }
        }
        let required = if query_tokens.len() == 1 {
            1
        } else {
            (query_tokens.len() * 3 + 2) / 5 // ceil(0.6 * n)
        };
        if matched_count < required {
            continue;
        }
        // 首个命中任一 query token 的 token 作为锚点
        let first = tokens.iter().find(|(start, end)| {
            let candidate: Vec<char> = normalize(&text.slice(*start, *end)).chars().collect();
            query_tokens.iter().any(|query_token| {
                edit_distance_within(query_token, &candidate, fuzzy_threshold(query_token.len()))
            })
        });
        if let Some((start, end)) = first {
            matches.push(Match {
                start: *start,
                end: *end,
            });
        }
    }
    matches
}

fn token_strings(value: &str) -> Vec<String> {
    static TOKEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").expect("static regex"));
    TOKEN
        .find_iter(value)
        .map(|matched| matched.as_str().to_string())
        .collect()
}

fn merge_ranges(mut ranges: Vec<Range>) -> Vec<Range> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(previous) if range.start <= previous.end => {
                previous.end = previous.end.max(range.end);
            }
            _ => merged.push(range),
        }
    }
    merged
}

fn context_ranges(text_len: usize, matches: &[Match]) -> Vec<Range> {
    let ranges = matches
        .iter()
        .map(|matched| Range {
            start: matched.start.saturating_sub(CONTEXT_CHARS),
            end: (matched.end + CONTEXT_CHARS).min(text_len),
        })
        .collect();
    merge_ranges(ranges)
}

/// 空白折叠后的长度(whitespaceSavings 前缀和,content-find.ts)。
struct WhitespaceIndex {
    starts: Vec<usize>,
    ends: Vec<usize>,
    savings: Vec<usize>,
}

impl WhitespaceIndex {
    fn new(runs: Vec<Range>) -> Self {
        let mut starts = Vec::with_capacity(runs.len());
        let mut ends = Vec::with_capacity(runs.len());
        let mut savings = vec![0usize];
        for run in &runs {
            starts.push(run.start);
            ends.push(run.end);
            let last = *savings.last().expect("non-empty");
            savings.push(last + run.end - run.start - 1);
        }
        WhitespaceIndex {
            starts,
            ends,
            savings,
        }
    }

    fn upper_bound(values: &[usize], target: usize) -> usize {
        let mut low = 0usize;
        let mut high = values.len();
        while low < high {
            let middle = (low + high) / 2;
            if values[middle] <= target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn lower_bound(values: &[usize], target: usize) -> usize {
        let mut low = 0usize;
        let mut high = values.len();
        while low < high {
            let middle = (low + high) / 2;
            if values[middle] < target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn normalized_length(&self, mut start: usize, mut end: usize) -> usize {
        let first_run = Self::upper_bound(&self.starts, start).wrapping_sub(1);
        if first_run < self.starts.len() && first_run < self.ends.len() && self.ends[first_run] > start
        {
            start = self.ends[first_run];
        }
        if start >= end {
            return 0;
        }
        let last_run = Self::upper_bound(&self.starts, end.saturating_sub(1)).wrapping_sub(1);
        if last_run < self.ends.len() && self.ends[last_run] >= end {
            end = self.starts[last_run];
        }
        if start >= end {
            return 0;
        }
        let first = Self::lower_bound(&self.starts, start);
        let last = Self::upper_bound(&self.ends, end);
        let savings_last = self.savings.get(last).copied().unwrap_or(0);
        let savings_first = self.savings.get(first).copied().unwrap_or(0);
        end - start - (savings_last - savings_first.min(savings_last))
    }
}

pub struct FindContentResult {
    pub text: String,
    pub match_count: usize,
    pub returned_matches: usize,
    pub query_results: Vec<(String, usize)>,
}

/// 检索入口(对齐 content-find.ts findContent 的有界摘排算法)。
pub fn find_content(text: &str, queries: &[String], mode: FindMode) -> FindContentResult {
    let mut normalized_queries: Vec<String> = Vec::new();
    for query in queries {
        let trimmed = query.trim().to_string();
        if !trimmed.is_empty() && !normalized_queries.contains(&trimmed) {
            normalized_queries.push(trimmed);
        }
    }
    let chars = CharText::new(text);
    let occurrences: Vec<(String, Vec<Match>)> = normalized_queries
        .iter()
        .map(|query| {
            let matches = match mode {
                FindMode::Fuzzy => fuzzy_matches(&chars, query),
                FindMode::Exact => literal_matches(&chars, query, false),
                FindMode::CaseInsensitive => literal_matches(&chars, query, true),
            };
            (query.clone(), matches)
        })
        .collect();
    let all_matches: Vec<Match> = occurrences
        .iter()
        .flat_map(|(_, matches)| matches.iter().cloned())
        .collect();
    let query_results: Vec<(String, usize)> = occurrences
        .iter()
        .map(|(query, matches)| (query.clone(), matches.len()))
        .collect();
    let total_matches = all_matches.len();

    let heading = if total_matches > 0 {
        format!("Text matches ({})", mode.as_str())
    } else {
        format!("Text matches ({}): no matches", mode.as_str())
    };
    let missing: Vec<String> = query_results
        .iter()
        .filter(|(_, count)| *count == 0)
        .map(|(query, _)| format!("\"{query}\""))
        .collect();
    let matching_queries: Vec<(usize, &String, &Vec<Match>)> = occurrences
        .iter()
        .enumerate()
        .filter(|(_, (_, matches))| !matches.is_empty())
        .map(|(index, (query, matches))| (index, query, matches))
        .collect();
    let whitespace_index = WhitespaceIndex::new(chars.whitespace_runs());
    let legend = if !matching_queries.is_empty() {
        format!(
            "Queries: {}",
            matching_queries
                .iter()
                .enumerate()
                .map(|(id_index, (_, query, _))| format!("Q{} = \"{query}\"", id_index + 1))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };
    let missing_notice = if missing.is_empty() {
        String::new()
    } else {
        format!("No matches: {}", missing.join(", "))
    };

    let measure = |ranges: &[Range], overflow: bool, omitted: &[usize]| -> (usize, usize) {
        let mut length = heading.chars().count();
        let mut returned_matches = 0usize;
        if overflow && !legend.is_empty() {
            length += 2 + legend.chars().count();
        }
        for (index, range) in ranges.iter().enumerate() {
            // 区段内每个命中 query 的计数(按首个命中位置、query 顺序排序)
            let mut counts: Vec<(usize, &String, usize, usize)> = matching_queries
                .iter()
                .filter_map(|(order, query, matches)| {
                    let starts: Vec<usize> = matches.iter().map(|matched| matched.start).collect();
                    let ends: Vec<usize> = matches.iter().map(|matched| matched.end).collect();
                    let first = WhitespaceIndex::lower_bound(&starts, range.start);
                    let last = WhitespaceIndex::upper_bound(&ends, range.end);
                    if last > first {
                        Some((*order, *query, matches[first].start, last - first))
                    } else {
                        None
                    }
                })
                .collect();
            if counts.is_empty() {
                continue;
            }
            counts.sort_by_key(|(_, _, first_start, order)| (*first_start, *order));
            let labels_length: usize = counts
                .iter()
                .map(|(order, query, _, count)| {
                    let label = if overflow {
                        format!("Q{}", order + 1).chars().count()
                    } else {
                        query.chars().count() + 2
                    };
                    label + 2 + count.to_string().chars().count()
                })
                .sum::<usize>()
                + 2 * (counts.len() - 1);
            let snippet_length = whitespace_index.normalized_length(range.start, range.end)
                + usize::from(range.start > 0)
                + usize::from(range.end < chars.len());
            length += 2
                + (index + 1).to_string().chars().count()
                + 2
                + labels_length
                + 1
                + snippet_length;
            returned_matches += counts.iter().map(|(_, _, _, count)| count).sum::<usize>();
        }
        if !missing_notice.is_empty() {
            length += 2 + missing_notice.chars().count();
        }
        if !omitted.is_empty() {
            length += 2
                + format!(
                    "No representative excerpt: {}.",
                    omitted
                        .iter()
                        .map(|order| format!("Q{}", order + 1))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .chars()
                .count();
        }
        if returned_matches < total_matches {
            length += 2
                + format!("Showing {returned_matches} of {total_matches} matches.")
                    .chars()
                    .count();
        }
        (length, returned_matches)
    };

    let format_ranges =
        |ranges: &[Range], overflow: bool, omitted: &[usize]| -> FindContentResult {
            let mut sections = vec![heading.clone()];
            let mut returned_matches = 0usize;
            if overflow && !legend.is_empty() {
                sections.push(legend.clone());
            }
            for (index, range) in ranges.iter().enumerate() {
                let mut counts: Vec<(usize, &String, usize)> = matching_queries
                    .iter()
                    .filter_map(|(order, query, matches)| {
                        let starts: Vec<usize> =
                            matches.iter().map(|matched| matched.start).collect();
                        let ends: Vec<usize> = matches.iter().map(|matched| matched.end).collect();
                        let first = WhitespaceIndex::lower_bound(&starts, range.start);
                        let last = WhitespaceIndex::upper_bound(&ends, range.end);
                        if last > first {
                            Some((*order, *query, last - first))
                        } else {
                            None
                        }
                    })
                    .collect();
                if counts.is_empty() {
                    continue;
                }
                counts.sort_by_key(|(_, _, count_order)| *count_order);
                let prefix = if range.start > 0 { "…" } else { "" };
                let suffix = if range.end < chars.len() { "…" } else { "" };
                let snippet = format!(
                    "{prefix}{}{suffix}",
                    chars
                        .slice(range.start, range.end)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                let counts_text = counts
                    .iter()
                    .map(|(order, query, count)| {
                        if overflow {
                            format!("Q{} ×{count}", order + 1)
                        } else {
                            format!("\"{query}\" ×{count}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                sections.push(format!("{}. {}\n{snippet}", index + 1, counts_text));
                returned_matches += counts.iter().map(|(_, _, count)| count).sum::<usize>();
            }
            if !missing_notice.is_empty() {
                sections.push(missing_notice.clone());
            }
            if !omitted.is_empty() {
                sections.push(format!(
                    "No representative excerpt: {}.",
                    omitted
                        .iter()
                        .map(|order| format!("Q{}", order + 1))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if returned_matches < total_matches {
                sections.push(format!(
                    "Showing {returned_matches} of {total_matches} matches."
                ));
            }
            FindContentResult {
                text: sections.join("\n\n"),
                match_count: total_matches,
                returned_matches,
                query_results: query_results.clone(),
            }
        };

    let full_ranges = context_ranges(chars.len(), &all_matches);
    let (full_length, full_returned) = measure(&full_ranges, false, &[]);
    if full_returned == total_matches && full_length <= MAX_OUTPUT_CHARS {
        return format_ranges(&full_ranges, false, &[]);
    }

    // 有界摘排:贪心为每个 query 选一个代表 witness
    let mut ranges: Vec<Range> = Vec::new();
    let mut omitted: Vec<usize> = matching_queries.iter().map(|(order, _, _)| *order).collect();
    let (_, zero_length) = measure(&ranges, true, &omitted);
    if zero_length > MAX_OUTPUT_CHARS {
        return FindContentResult {
            text: format!(
                "{heading}\n\nUnable to format bounded excerpts: query metadata exceeds {MAX_OUTPUT_CHARS} characters.\n\nShowing 0 of {total_matches} matches."
            ),
            match_count: total_matches,
            returned_matches: 0,
            query_results,
        };
    }
    struct Witness {
        matched: Match,
    }
    let mut witnesses: Vec<Witness> = Vec::new();
    for (order, _, matches) in &matching_queries {
        let proposed_omitted: Vec<usize> =
            omitted.iter().copied().filter(|id| id != order).collect();
        let (current_length, _) = measure(&ranges, true, &omitted);
        let mut selected: Option<(Match, Vec<Range>, usize)> = None;
        for matched in matches.iter() {
            let proposed_ranges = merge_ranges(
                ranges
                    .iter()
                    .copied()
                    .chain(std::iter::once(Range {
                        start: matched.start,
                        end: matched.end,
                    }))
                    .collect(),
            );
            let (summary_length, _) = measure(&proposed_ranges, true, &proposed_omitted);
            let cost = summary_length - current_length.min(summary_length);
            let better = match &selected {
                None => summary_length <= MAX_OUTPUT_CHARS,
                Some((selected_matched, _, selected_cost)) => {
                    summary_length <= MAX_OUTPUT_CHARS
                        && (cost < *selected_cost
                            || (cost == *selected_cost
                                && (matched.start < selected_matched.start
                                    || (matched.start == selected_matched.start
                                        && matched.end < selected_matched.end))))
                }
            };
            if better {
                selected = Some((matched.clone(), proposed_ranges, cost));
            }
        }
        if let Some((matched, proposed_ranges, _)) = selected {
            ranges = proposed_ranges;
            omitted = proposed_omitted;
            witnesses.push(Witness { matched });
        }
    }
    for witness in &witnesses {
        let proposed = merge_ranges(
            ranges
                .iter()
                .copied()
                .chain(std::iter::once(Range {
                    start: witness.matched.start.saturating_sub(CONTEXT_CHARS),
                    end: (witness.matched.end + CONTEXT_CHARS).min(chars.len()),
                }))
                .collect(),
        );
        let (length, _) = measure(&proposed, true, &omitted);
        if length <= MAX_OUTPUT_CHARS {
            ranges = proposed;
        }
    }
    let all_occurrences = merge_ranges(
        ranges
            .iter()
            .copied()
            .chain(all_matches.iter().map(|matched| Range {
                start: matched.start,
                end: matched.end,
            }))
            .collect(),
    );
    let (length, _) = measure(&all_occurrences, true, &omitted);
    if length <= MAX_OUTPUT_CHARS {
        ranges = all_occurrences;
    }
    format_ranges(&ranges, true, &omitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queries(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn exact_mode_finds_and_bounds_context() {
        let text = format!("{}target{}", "x".repeat(1000), "y".repeat(1000));
        let result = find_content(&text, &queries(&["target"]), FindMode::Exact);
        assert_eq!(result.match_count, 1);
        assert!(result.text.contains("Text matches (exact)"));
        assert!(result.text.contains("target"));
        // 上下文 400*2 + 命中 6 + 前后省略号,长度 < 20k
        assert!(result.text.chars().count() < 1000);
    }

    #[test]
    fn case_insensitive_default_behavior() {
        let result = find_content("Hello World", &queries(&["hello world"]), FindMode::CaseInsensitive);
        assert_eq!(result.match_count, 1);
        let exact = find_content("Hello World", &queries(&["hello world"]), FindMode::Exact);
        assert_eq!(exact.match_count, 0);
    }

    #[test]
    fn fuzzy_tolerates_typos_and_accents() {
        let text = "The implementation is fully documentted here.\n\nUnrelated paragraph.";
        let result = find_content(text, &queries(&["documented"]), FindMode::Fuzzy);
        assert_eq!(result.match_count, 1);
        let accents = find_content(
            "café culture",
            &queries(&["cafe culture"]),
            FindMode::Fuzzy,
        );
        assert_eq!(accents.match_count, 1);
    }

    #[test]
    fn multiple_queries_reported() {
        let text = "alpha beta gamma";
        let result = find_content(
            text,
            &queries(&["alpha", "gamma", "missing"]),
            FindMode::Exact,
        );
        assert_eq!(result.match_count, 2);
        assert_eq!(result.query_results.len(), 3);
        assert_eq!(result.query_results[2].1, 0);
        assert!(result.text.contains("No matches: \"missing\""));
    }

    #[test]
    fn overflow_switches_to_query_ids() {
        // 大量重复命中 + 巨大文本 → 走 overflow 布局(Q1 形式)
        let text = format!("needle {}", "filler ".repeat(3000));
        let queries = (0..3).map(|index| format!("needle{index}")).collect::<Vec<_>>();
        let mut all = queries.clone();
        all.push("needle".to_string());
        let result = find_content(&text, &all, FindMode::Exact);
        assert!(result.text.chars().count() <= MAX_OUTPUT_CHARS + 200);
        assert_eq!(result.match_count, 1);
    }
}
