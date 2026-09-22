-- ZCode 外部来源适配器的手写脱敏夹具：表结构逐字取自本机 ZCode 3.14.3（CLI 引擎
-- 0.16.9，迁移 0022）的 schema，数据全部是编造的。测试以 `sqlite3 <db>` 从 stdin
-- 灌入本文件建库；时间以 NOW = 1790000000000（2026-09-21T14:13:20Z）为基准。
CREATE TABLE schema_migration (
      id text primary key,
      checksum text not null,
      app_version text,
      time_applied integer not null
    );
CREATE TABLE session (
        id text primary key,
        project_id text not null,
        workspace_id text,
        parent_id text,
        slug text not null,
        directory text not null,
        path text,
        title text not null,
        version text not null,
        share_url text,
        summary_additions integer,
        summary_deletions integer,
        summary_files integer,
        summary_diffs text,
        revert text,
        permission text,
        time_created integer not null,
        time_updated integer not null,
        time_compacting integer,
        time_archived integer
      , task_type text not null default 'interactive', title_source text not null default 'first_input'
        check(title_source in ('default', 'first_input', 'generated', 'custom')), title_message_id text, time_title_updated integer, trace_id text);
CREATE INDEX session_project_idx on session(project_id);
CREATE INDEX session_workspace_idx on session(workspace_id);
CREATE INDEX session_parent_idx on session(parent_id);
CREATE INDEX session_task_type_idx on session(task_type);
CREATE INDEX session_trace_idx on session(trace_id);
CREATE TABLE turn_usage (
        session_id text not null references session(id) on delete cascade,
        turn_id text not null,
        trace_id text,
        user_message_id text,
        status text not null check(status in ('running', 'completed', 'error', 'cancelled')),
        started_at integer not null,
        first_model_start_at integer,
        first_token_at integer,
        completed_at integer,
        duration_ms integer,
        time_to_first_token_ms integer,
        model_request_count integer not null default 0,
        model_retry_count integer not null default 0,
        tool_call_count integer not null default 0,
        tool_error_count integer not null default 0,
        input_tokens integer not null default 0,
        output_tokens integer not null default 0,
        reasoning_tokens integer not null default 0,
        cache_creation_input_tokens integer not null default 0,
        cache_read_input_tokens integer not null default 0,
        computed_total_tokens integer not null default 0,
        retryable integer not null default 0 check(retryable in (0, 1)),
        cancelled_by_user integer not null default 0 check(cancelled_by_user in (0, 1)),
        context_exceeded integer not null default 0 check(context_exceeded in (0, 1)),
        error_type text,
        error_code text,
        primary key(session_id, turn_id)
      );
CREATE TABLE todo (
        session_id text not null references session(id) on delete cascade,
        content text not null,
        status text not null,
        priority text not null,
        position integer not null,
        time_created integer not null,
        time_updated integer not null,
        primary key(session_id, position)
      );
CREATE INDEX todo_session_idx on todo(session_id);

INSERT INTO schema_migration VALUES
  ('0001_base_session_store', 'x', '0.2.0', 1784516086695),
  ('0010_usage_observability', 'x', '0.15.0', 1784516086707),
  ('0022_backfilled_session_reasoning', 'x', '0.16.5', 1789544430378);

-- 根会话 A：近期、在跑（最后一轮 running）；它下面挂齐各种子节点。
INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, task_type)
VALUES ('sess_a0000000-0000-4000-8000-000000000001', 'proj_demo', NULL, 'refactor-parser', '/work/demo',
        'Refactor the parser', '0.16.9', 1789996400000, 1789999940000, 'interactive');
-- 根会话 B：标题为空、最后一轮是 5 小时前崩掉留下的 running（陈旧）。
INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, task_type)
VALUES ('sess_b0000000-0000-4000-8000-000000000002', 'proj_demo', NULL, 'untitled', '/work/other',
        '', '0.16.9', 1789892000000, 1789982000000, 'interactive');
-- 根会话 OLD：5 天前，落在近期窗口之外。
INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, task_type)
VALUES ('sess_c0000000-0000-4000-8000-000000000003', 'proj_demo', NULL, 'old', '/work/old',
        'Old session', '0.16.5', 1789560000000, 1789568000000, 'interactive');
-- 根会话 ARCHIVED：近期但已归档。
INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, time_archived, task_type)
VALUES ('sess_d0000000-0000-4000-8000-000000000004', 'proj_demo', NULL, 'archived', '/work/archived',
        'Archived session', '0.16.9', 1789999000000, 1789999400000, 1789999700000, 'interactive');

-- A 的子会话。S1 完成（有转录与 output.txt），S8 嵌在 S1 下、S9 嵌在 S8 下。
INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, task_type) VALUES
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000001', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   's1', '/work/demo', 'Explore: map parser', '0.16.3', 1789997000000, 1789997600000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000002', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   's2', '/work/demo', 'general-purpose: run tests', '0.16.9', 1789997100000, 1789997200000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000003', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   's3', '/work/demo', 'Explore: scan call sites', '0.16.9', 1789999400000, 1789999970000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000004', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   's4', '/work/demo', 'Check lints', '0.16.9', 1789998000000, 1789998100000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000005', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   's5', '/work/demo', 'Draft summary', '0.16.9', 1789998200000, 1789998250000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000006', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   's6', '/work/demo', 'Inspect fixtures', '0.16.9', 1789998300000, 1789998350000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000007', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   's7', '/work/demo', 'Background: watch build', '0.16.9', 1789998400000, 1789998500000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000008', 'proj_demo', 'sess_subagent_agent_10000000-0000-4000-8000-000000000001',
   's8', '/work/demo', 'Explore: tokenizer', '0.16.3', 1789997300000, 1789997400000, 'subagent_child'),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000009', 'proj_demo', 'sess_subagent_agent_10000000-0000-4000-8000-000000000008',
   's9', '/work/demo', 'Explore: lexer tables', '0.16.3', 1789997350000, 1789997380000, 'subagent_child'),
  ('sess_e0000000-0000-4000-8000-000000000005', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   'side', '/work/demo', 'Explain this selection', '0.16.9', 1789999000000, 1789999050000, 'selection_side_chat'),
  ('sess_f0000000-0000-4000-8000-000000000006', 'proj_demo', 'sess_a0000000-0000-4000-8000-000000000001',
   'future', '/work/demo', 'Future step', '0.16.9', 1789999100000, 1789999150000, 'workflow_step');

-- B 的子会话：metadata 停在 running，六小时没有动静（陈旧）。
INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, task_type) VALUES
  ('sess_subagent_agent_10000000-0000-4000-8000-00000000000a', 'proj_demo', 'sess_b0000000-0000-4000-8000-000000000002',
   's10', '/work/other', 'Explore: stale run', '0.16.9', 1789978400000, 1789978400000, 'subagent_child');

-- OLD 的子会话：随根一起被窗口排除。
INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, task_type) VALUES
  ('sess_subagent_agent_10000000-0000-4000-8000-00000000000b', 'proj_demo', 'sess_c0000000-0000-4000-8000-000000000003',
   's11', '/work/old', 'Explore: old', '0.16.5', 1789560000000, 1789561000000, 'subagent_child');

INSERT INTO turn_usage (session_id, turn_id, status, started_at, completed_at, computed_total_tokens) VALUES
  ('sess_a0000000-0000-4000-8000-000000000001', 'turn_a1', 'completed', 1789996500000, 1789996600000, 1000),
  ('sess_a0000000-0000-4000-8000-000000000001', 'turn_a2', 'running', 1789999880000, NULL, 0),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000003', 'turn_s3', 'running', 1789999400000, NULL, 0),
  -- S4 先有一轮崩溃留下的 running，之后又完成了一轮：只看最新一轮。
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000004', 'turn_s4_crashed', 'running', 1789997900000, NULL, 0),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000004', 'turn_s4', 'completed', 1789998000000, 1789998050000, 4200),
  ('sess_b0000000-0000-4000-8000-000000000002', 'turn_b1', 'running', 1789982000000, NULL, 0);

INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated) VALUES
  ('sess_a0000000-0000-4000-8000-000000000001', 'Map the parser entry points', 'completed', 'high', 0, 1789996500000, 1789997700000),
  ('sess_a0000000-0000-4000-8000-000000000001', 'Split the tokenizer', 'in_progress', 'high', 1, 1789996500000, 1789999900000),
  ('sess_a0000000-0000-4000-8000-000000000001', 'Add regression tests', 'pending', 'medium', 2, 1789996500000, 1789996500000),
  ('sess_a0000000-0000-4000-8000-000000000001', 'Tidy the docs', 'deferred', 'low', 3, 1789996500000, 1789996500000),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000003', 'Scan call sites', 'in_progress', 'medium', 0, 1789999400000, 1789999500000),
  ('sess_subagent_agent_10000000-0000-4000-8000-000000000001', 'Read the parser module', 'completed', 'high', 0, 1789997000000, 1789997500000);
