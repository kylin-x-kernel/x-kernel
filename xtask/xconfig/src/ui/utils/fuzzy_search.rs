use crate::ui::state::MenuItem;

pub struct FuzzySearcher {
    query: String,
    case_sensitive: bool,
}

impl FuzzySearcher {
    pub fn new(query: String) -> Self {
        Self {
            query,
            case_sensitive: false,
        }
    }

    pub fn search(&self, items: &[MenuItem]) -> Vec<SearchResult> {
        if self.query.is_empty() {
            return Vec::new();
        }

        let mut results: Vec<SearchResult> = items
            .iter()
            .filter_map(|item| {
                let score = self.calculate_score(&item.label, &item.id);
                if score > 0 {
                    Some(SearchResult {
                        item: item.clone(),
                        score,
                    })
                } else {
                    None
                }
            })
            .collect();

        results.sort_by(|a, b| b.score.cmp(&a.score));
        results
    }

    fn calculate_score(&self, label: &str, id: &str) -> i32 {
        let query = if self.case_sensitive {
            self.query.clone()
        } else {
            self.query.to_lowercase()
        };

        let label_lower = if self.case_sensitive {
            label.to_string()
        } else {
            label.to_lowercase()
        };

        let id_lower = if self.case_sensitive {
            id.to_string()
        } else {
            id.to_lowercase()
        };

        let mut score = 0;

        // Exact match
        if label_lower == query || id_lower == query {
            return 1000;
        }

        // Contains match
        if label_lower.contains(&query) {
            score += 100;
            // Bonus for match at start
            if label_lower.starts_with(&query) {
                score += 50;
            }
        }

        if id_lower.contains(&query) {
            score += 90;
            if id_lower.starts_with(&query) {
                score += 50;
            }
        }

        // Fuzzy matching - check if all query chars appear in order
        if score == 0 {
            let mut label_chars = label_lower.chars();
            let mut matched = 0;

            for query_char in query.chars() {
                if let Some(_) = label_chars.find(|&c| c == query_char) {
                    matched += 1;
                }
            }

            if matched == query.len() {
                score = 10 + matched as i32;
            }
        }

        score
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub item: MenuItem,
    pub score: i32,
}
