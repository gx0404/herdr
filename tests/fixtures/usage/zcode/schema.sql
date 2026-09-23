-- ZCode 本地用量来源的手写脱敏夹具（表结构）：`model_usage` 的建表与索引语句逐字取自
-- 本机 ZCode 3.14.3（CLI 引擎 0.16.9，迁移 0022_backfilled_session_reasoning）的
-- `.schema model_usage`；只有表结构，没有任何行。外键指向的 `session` 表不建：sqlite3
-- 默认不开外键约束，本来源也只读这一张表。
CREATE TABLE model_usage (
        id text primary key,
        logical_request_id text not null,
        attempt_index integer not null default 0,
        session_id text not null references session(id) on delete cascade,
        turn_id text,
        trace_id text,
        span_id text,
        assistant_message_id text,
        parent_user_message_id text,
        query_source text not null,
        provider_id text not null,
        model_id text not null,
        variant text,
        agent text,
        mode text,
        task_type text,
        status text not null check(status in ('running', 'completed', 'error', 'cancelled')),
        started_at integer not null,
        first_token_at integer,
        completed_at integer,
        duration_ms integer,
        time_to_first_token_ms integer,
        finish_reason text,
        tool_call_count integer not null default 0,
        input_tokens integer not null default 0,
        output_tokens integer not null default 0,
        reasoning_tokens integer not null default 0,
        cache_creation_input_tokens integer not null default 0,
        cache_read_input_tokens integer not null default 0,
        provider_total_tokens integer,
        computed_total_tokens integer not null default 0,
        retry_count integer not null default 0,
        retryable integer not null default 0 check(retryable in (0, 1)),
        cancelled_by_user integer not null default 0 check(cancelled_by_user in (0, 1)),
        context_exceeded integer not null default 0 check(context_exceeded in (0, 1)),
        error_type text,
        error_code text,
        error_message text,
        raw_usage_json text,
        provider_metadata_json text
      );
CREATE INDEX model_usage_started_model_idx
        on model_usage(started_at, provider_id, model_id);
CREATE INDEX model_usage_session_turn_idx
        on model_usage(session_id, turn_id);
CREATE INDEX model_usage_trace_idx
        on model_usage(trace_id);
CREATE INDEX model_usage_query_source_idx
        on model_usage(query_source);
