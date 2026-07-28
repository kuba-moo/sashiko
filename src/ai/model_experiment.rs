// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

use super::{AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities};
use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const ROUTE_PREFIX: &str = "[model-route:";
pub const PROMPT_PREFIX_CACHE_SOURCES_ENV: &str = "SASHIKO_PROMPT_PREFIX_CACHE_SOURCES";

/// Makes review-level experiment cohort selection stable across retries and processes.
pub fn sampled_for_review(probability: f64, review_id: i64, patch_id: i64, name: &str) -> bool {
    if probability <= 0.0 {
        return false;
    }
    if probability >= 1.0 {
        return true;
    }
    let digest = Sha256::digest(format!("{review_id}:{patch_id}:{name}").as_bytes());
    let bucket = u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]));
    (bucket as f64 / u64::MAX as f64) < probability
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceIdentity {
    pub name: String,
    pub display_name: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CohortMember {
    pub source: SourceIdentity,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReviewCohort {
    pub main: SourceIdentity,
    pub variants: Vec<CohortMember>,
}

impl ReviewCohort {
    pub fn selected_variants(&self) -> impl Iterator<Item = &SourceIdentity> {
        self.variants
            .iter()
            .filter(|member| member.selected)
            .map(|member| &member.source)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.selected_variants().any(|source| source.name == name)
    }
}

pub struct UnavailableProvider {
    error: String,
    model: String,
}

impl UnavailableProvider {
    pub fn new(error: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl AiProvider for UnavailableProvider {
    async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
        anyhow::bail!(self.error.clone())
    }

    fn estimate_tokens(&self, _request: &AiRequest) -> usize {
        0
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: 0,
        }
    }
}

/// Encodes a provider route in the existing context tag carried over stdio.
pub struct RoutedProvider {
    inner: Arc<dyn AiProvider>,
    route: String,
}

impl RoutedProvider {
    pub fn new(inner: Arc<dyn AiProvider>, route: impl Into<String>) -> Self {
        Self {
            inner,
            route: route.into(),
        }
    }
}

#[async_trait]
impl AiProvider for RoutedProvider {
    async fn generate_content(&self, mut request: AiRequest) -> Result<AiResponse> {
        let tag = request.context_tag.get_or_insert_default();
        tag.insert_str(0, &format!("{ROUTE_PREFIX}{}] ", self.route));
        self.inner.generate_content(request).await
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        self.inner.estimate_tokens(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        self.inner.cache_stats()
    }

    fn caches_prompt_prefix(&self) -> bool {
        self.inner.caches_prompt_prefix()
    }
}

/// Removes and returns a model route from an incoming stdio request.
pub fn take_route(context_tag: &mut Option<String>) -> Option<String> {
    let tag = context_tag.as_mut()?;
    let rest = tag.strip_prefix(ROUTE_PREFIX)?;
    let end = rest.find(']')?;
    let route = rest[..end].to_string();
    tag.drain(..ROUTE_PREFIX.len() + end + 2);
    Some(route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_is_stable_and_honors_boundaries() {
        assert!(!sampled_for_review(0.0, 1, 2, "test"));
        assert!(sampled_for_review(1.0, 1, 2, "test"));
        assert_eq!(
            sampled_for_review(0.5, 10, 20, "variant"),
            sampled_for_review(0.5, 10, 20, "variant")
        );
    }

    #[test]
    fn route_round_trips_without_losing_context() {
        let mut tag = Some("[model-route:variant] [ps:1 p:2 s:3] ".to_string());
        assert_eq!(take_route(&mut tag).as_deref(), Some("variant"));
        assert_eq!(tag.as_deref(), Some("[ps:1 p:2 s:3] "));
    }

    #[test]
    fn cohort_exposes_only_selected_variants() {
        let cohort = ReviewCohort {
            main: SourceIdentity {
                name: "main".into(),
                display_name: "primary".into(),
                provider: "openai".into(),
                model: "main-model".into(),
            },
            variants: vec![
                CohortMember {
                    source: SourceIdentity {
                        name: "selected".into(),
                        display_name: "selected".into(),
                        provider: "claude".into(),
                        model: "variant-a".into(),
                    },
                    selected: true,
                },
                CohortMember {
                    source: SourceIdentity {
                        name: "control".into(),
                        display_name: "control".into(),
                        provider: "gemini".into(),
                        model: "variant-b".into(),
                    },
                    selected: false,
                },
            ],
        };

        assert!(cohort.contains("selected"));
        assert!(!cohort.contains("control"));
    }
}
