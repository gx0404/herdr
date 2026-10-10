//! 统一状态语义：一个 agent 此刻属于哪个「桶」。
//!
//! 生命周期（`AgentStatus`）与注意力（失败锁存）分开记录，桶由这一个纯函数推导；
//! 侧栏、标签、标题栏计数、总览、通知全部按桶取字形、颜色与排序，不再各自映射。
//! 借鉴 Paseo 的 `deriveAgentStateBucket`：需要输入不锁存（由当前状态实时推导），
//! 失败锁存到用户看过为止，未知不等于空闲（不画指示、不参与汇总）。
// 列表顺序、计数与汇总由会话侧栏、总览与标题栏计数（同一改版的后续提交）接入。
#![allow(dead_code)]

use crate::api::schema::AgentStatus;

/// 一个 agent（或汇总后的工作区 / 标签）在界面上的状态桶。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentBucket {
    /// 等待权限、提问或确认（`AgentStatus::Blocked`）。
    NeedsInput,
    /// 出错结束或异常退出，用户还没看过。
    Failed,
    /// 正在运行。
    Working,
    /// 已完成、还没看过（`AgentStatus::Done`）。
    Review,
    /// 看过的空闲。
    Idle,
    /// 不是 agent，或状态未知：不画指示、不参与汇总。
    None,
}

impl AgentBucket {
    /// 按生命周期与失败锁存推导。需要输入优先于失败：等待权限的 agent 即便上一轮
    /// 失败过，此刻要你处理的也是这个请求。
    pub fn derive(status: AgentStatus, failed: bool) -> Self {
        match status {
            AgentStatus::Blocked => Self::NeedsInput,
            _ if failed => Self::Failed,
            AgentStatus::Working => Self::Working,
            AgentStatus::Done => Self::Review,
            AgentStatus::Idle => Self::Idle,
            AgentStatus::Unknown => Self::None,
        }
    }

    /// 汇总优先级（工作区行、标签页、标题栏）：数值越大越优先。运行中排在待查看
    /// 之前，忙碌的工作区保持转圈。
    pub fn rollup_rank(self) -> u8 {
        match self {
            Self::NeedsInput => 5,
            Self::Failed => 4,
            Self::Working => 3,
            Self::Review => 2,
            Self::Idle => 1,
            Self::None => 0,
        }
    }

    /// 列表顺序（按状态分组、总览）：数值越小越靠前。要你处理的排最前，待查看
    /// 排在运行中之前。
    pub fn listing_rank(self) -> u8 {
        match self {
            Self::NeedsInput => 0,
            Self::Failed => 1,
            Self::Review => 2,
            Self::Working => 3,
            Self::Idle => 4,
            Self::None => 5,
        }
    }

    /// 需要用户处理（下一个待处理、注意力计数）的桶。
    pub fn wants_attention(self) -> bool {
        matches!(self, Self::NeedsInput | Self::Failed | Self::Review)
    }

    /// 按汇总优先级取最高的桶；空集合为 `None`。
    pub fn rollup(buckets: impl IntoIterator<Item = AgentBucket>) -> Self {
        buckets
            .into_iter()
            .max_by_key(|bucket| bucket.rollup_rank())
            .unwrap_or(Self::None)
    }

    /// 稳定的英文标识（API / 偏好里用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsInput => "needs_input",
            Self::Failed => "failed",
            Self::Working => "working",
            Self::Review => "review",
            Self::Idle => "idle",
            Self::None => "none",
        }
    }

    /// 总览 / 按状态分组的组顺序。
    pub const LISTING: [AgentBucket; 5] = [
        Self::NeedsInput,
        Self::Failed,
        Self::Review,
        Self::Working,
        Self::Idle,
    ];
}

/// 各桶的数量，用于标题栏与总览头部的计数行。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketCounts {
    pub needs_input: usize,
    pub failed: usize,
    pub working: usize,
    pub review: usize,
    pub idle: usize,
}

impl BucketCounts {
    pub fn add(&mut self, bucket: AgentBucket) {
        match bucket {
            AgentBucket::NeedsInput => self.needs_input += 1,
            AgentBucket::Failed => self.failed += 1,
            AgentBucket::Working => self.working += 1,
            AgentBucket::Review => self.review += 1,
            AgentBucket::Idle => self.idle += 1,
            AgentBucket::None => {}
        }
    }

    pub fn get(&self, bucket: AgentBucket) -> usize {
        match bucket {
            AgentBucket::NeedsInput => self.needs_input,
            AgentBucket::Failed => self.failed,
            AgentBucket::Working => self.working,
            AgentBucket::Review => self.review,
            AgentBucket::Idle => self.idle,
            AgentBucket::None => 0,
        }
    }

    pub fn total(&self) -> usize {
        self.needs_input + self.failed + self.working + self.review + self.idle
    }
}

impl FromIterator<AgentBucket> for BucketCounts {
    fn from_iter<T: IntoIterator<Item = AgentBucket>>(iter: T) -> Self {
        let mut counts = Self::default();
        for bucket in iter {
            counts.add(bucket);
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_maps_lifecycle_and_failure() {
        let cases = [
            (AgentStatus::Blocked, false, AgentBucket::NeedsInput),
            (AgentStatus::Blocked, true, AgentBucket::NeedsInput),
            (AgentStatus::Working, true, AgentBucket::Failed),
            (AgentStatus::Idle, true, AgentBucket::Failed),
            (AgentStatus::Working, false, AgentBucket::Working),
            (AgentStatus::Done, false, AgentBucket::Review),
            (AgentStatus::Idle, false, AgentBucket::Idle),
            (AgentStatus::Unknown, false, AgentBucket::None),
        ];
        for (status, failed, expected) in cases {
            assert_eq!(
                AgentBucket::derive(status, failed),
                expected,
                "{status:?} failed={failed}"
            );
        }
    }

    #[test]
    fn rollup_keeps_working_above_review_and_ignores_unknown() {
        assert_eq!(
            AgentBucket::rollup([AgentBucket::Review, AgentBucket::Working, AgentBucket::Idle]),
            AgentBucket::Working
        );
        assert_eq!(
            AgentBucket::rollup([AgentBucket::Working, AgentBucket::Failed]),
            AgentBucket::Failed
        );
        assert_eq!(
            AgentBucket::rollup([AgentBucket::Failed, AgentBucket::NeedsInput]),
            AgentBucket::NeedsInput
        );
        assert_eq!(AgentBucket::rollup([AgentBucket::None]), AgentBucket::None);
        assert_eq!(AgentBucket::rollup([]), AgentBucket::None);
    }

    #[test]
    fn listing_puts_actionable_buckets_first() {
        let mut buckets = vec![
            AgentBucket::Idle,
            AgentBucket::Working,
            AgentBucket::Review,
            AgentBucket::Failed,
            AgentBucket::NeedsInput,
        ];
        buckets.sort_by_key(|bucket| bucket.listing_rank());
        assert_eq!(buckets, AgentBucket::LISTING.to_vec());
    }

    #[test]
    fn counts_collect_every_bucket_but_none() {
        let counts: BucketCounts = [
            AgentBucket::NeedsInput,
            AgentBucket::Working,
            AgentBucket::Working,
            AgentBucket::Review,
            AgentBucket::None,
        ]
        .into_iter()
        .collect();
        assert_eq!(counts.needs_input, 1);
        assert_eq!(counts.working, 2);
        assert_eq!(counts.review, 1);
        assert_eq!(counts.total(), 4);
        assert_eq!(counts.get(AgentBucket::None), 0);
    }

    #[test]
    fn attention_buckets_are_the_actionable_ones() {
        assert!(AgentBucket::NeedsInput.wants_attention());
        assert!(AgentBucket::Failed.wants_attention());
        assert!(AgentBucket::Review.wants_attention());
        assert!(!AgentBucket::Working.wants_attention());
        assert!(!AgentBucket::Idle.wants_attention());
        assert!(!AgentBucket::None.wants_attention());
    }
}
