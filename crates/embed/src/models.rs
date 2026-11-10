//! The reranking models MemoryOS can run. Each one is pinned to a Hugging Face revision, so every
//! machine downloads the same files, and only these can be chosen: no setting takes a URL.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RerankModel {
    /// The most accurate on the bench, and the only multilingual one.
    #[default]
    BgeRerankerV2M3,
    BgeRerankerBase,
    JinaRerankerV1TurboEn,
    MsMarcoMiniLmL6,
}

impl RerankModel {
    pub const ALL: [Self; 4] = [
        Self::BgeRerankerV2M3,
        Self::BgeRerankerBase,
        Self::JinaRerankerV1TurboEn,
        Self::MsMarcoMiniLmL6,
    ];

    /// The name settings and the command line use.
    pub fn name(self) -> &'static str {
        match self {
            Self::BgeRerankerV2M3 => "bge-reranker-v2-m3",
            Self::BgeRerankerBase => "bge-reranker-base",
            Self::JinaRerankerV1TurboEn => "jina-reranker-v1-turbo-en",
            Self::MsMarcoMiniLmL6 => "ms-marco-minilm-l6",
        }
    }

    pub fn languages(self) -> &'static str {
        match self {
            Self::BgeRerankerV2M3 => "multilingual",
            Self::BgeRerankerBase => "English, Chinese",
            Self::JinaRerankerV1TurboEn | Self::MsMarcoMiniLmL6 => "English",
        }
    }

    /// The int8 model file's download size.
    pub fn size_mb(self) -> u32 {
        match self {
            Self::BgeRerankerV2M3 => 571,
            Self::BgeRerankerBase => 279,
            Self::JinaRerankerV1TurboEn => 38,
            Self::MsMarcoMiniLmL6 => 23,
        }
    }

    pub(crate) fn repo(self) -> &'static str {
        match self {
            Self::BgeRerankerV2M3 => "onnx-community/bge-reranker-v2-m3-ONNX",
            Self::BgeRerankerBase => "Xenova/bge-reranker-base",
            Self::JinaRerankerV1TurboEn => "jinaai/jina-reranker-v1-turbo-en",
            Self::MsMarcoMiniLmL6 => "Xenova/ms-marco-MiniLM-L-6-v2",
        }
    }

    pub(crate) fn revision(self) -> &'static str {
        match self {
            Self::BgeRerankerV2M3 => "6f5ff65298512715a1e669753bc754d2bc8f367b",
            Self::BgeRerankerBase => "280bcc27a84e0b898c251e06fddb25171bd9b101",
            Self::JinaRerankerV1TurboEn => "b8c14f4e723d9e0aab4732a7b7b93741eeeb77c2",
            Self::MsMarcoMiniLmL6 => "a09144355adeed5f58c8ed011d209bf8ee5a1fec",
        }
    }

    /// Every repo holds the same int8 export at the same path.
    pub(crate) fn file(self) -> &'static str {
        "onnx/model_int8.onnx"
    }
}

impl fmt::Display for RerankModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for RerankModel {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        Self::ALL
            .into_iter()
            .find(|model| model.name().eq_ignore_ascii_case(text))
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|model| model.name()).collect();
                format!(
                    "unknown reranking model {text:?}, choose one of: {}",
                    names.join(", ")
                )
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_name_parses_back_to_its_model() {
        for model in RerankModel::ALL {
            assert_eq!(model.name().parse::<RerankModel>().unwrap(), model);
            assert_eq!(model.to_string(), model.name());
        }
        assert_eq!(
            " BGE-Reranker-Base ".parse::<RerankModel>().unwrap(),
            RerankModel::BgeRerankerBase
        );
    }

    #[test]
    fn an_unknown_name_lists_the_choices() {
        let error = "gpt".parse::<RerankModel>().unwrap_err();
        assert!(error.contains("bge-reranker-v2-m3"), "{error}");
    }

    #[test]
    fn the_default_is_the_model_recall_used_before_the_choice_existed() {
        assert_eq!(RerankModel::default(), RerankModel::BgeRerankerV2M3);
        assert_eq!(
            RerankModel::default().repo(),
            "onnx-community/bge-reranker-v2-m3-ONNX"
        );
    }
}
