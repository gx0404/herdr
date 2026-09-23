-- ZCode 本地用量来源的手写脱敏夹具（数据）：每一行都是编造的，只填本来源读到的列与
-- 必填列，不含任何对话正文、提示词或凭据（`error_message` / `raw_usage_json` /
-- `provider_metadata_json` 一律留空）。时间以 NOW = 1790000000000
-- （2026-09-21T14:13:20Z）为基准，统计窗口是 [NOW - 24 h, NOW]，下界
-- 1789913600000 含在窗口内。
--
-- 预期（`zcode_local` 的端到端用例逐项断言）：
--   主任务 token = 12800 + 21500 + 320 + 5400 + 1000 + 0（坏行）+ 0（running）+ 7（下界）= 41027
--   子 agent token = 8600 + 9700 + 4300 + 1600 = 24200；合计 65227
--   工具调用 = 3 + 2 + 1 + 5 + 4 + 2 + 1 = 18（坏行的 'n/a' 按 0 计）
--   子 agent 数 = 3（会话 01 / 02 / 03；04 只有窗口外的行）

-- 主任务：main_turn / session_title / compact 都算主任务（task_type = interactive）。
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0001', 'lr_0001', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'completed', 1789989200000, 1789989260000, 3, 12000, 800, 50, 9000, 12800, 12800);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0002', 'lr_0002', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'completed', 1789996400000, 1789996460000, 2, 20000, 1500, 0, 15000, NULL, 21500);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0003', 'lr_0003', 'sess_a0000000-0000-4000-8000-000000000001', 'session_title', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'completed', 1789992800000, 1789992801000, 0, 300, 20, 0, 0, 320, 320);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0004', 'lr_0004', 'sess_a0000000-0000-4000-8000-000000000001', 'compact', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'completed', 1789998200000, 1789998230000, 0, 5000, 400, 0, 3000, 5400, 5400);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens, cancelled_by_user)
VALUES ('mu_0005', 'lr_0005', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'cancelled', 1789999400000, 1789999405000, 1, 1000, 0, 0, 0, NULL, 1000, 1);
-- 仍在跑的请求：token 还没回填。
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, tool_call_count, input_tokens, output_tokens, computed_total_tokens)
VALUES ('mu_0006', 'lr_0006', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'running', 1789999940000, 0, 0, 0, 0);
-- 坏行：整数列里混进了文本（sqlite 的动态类型允许），聚合按 0 计，不拖垮整份结果。
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, tool_call_count, input_tokens, output_tokens, computed_total_tokens)
VALUES ('mu_0007', 'lr_0007', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'error', 1789999700000, 'n/a', 'n/a', 'n/a', 'garbage');
-- 窗口下界：恰好 NOW - 24 h 的算进来，早 1 ms 的不算。
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, tool_call_count, input_tokens, output_tokens, computed_total_tokens)
VALUES ('mu_0008', 'lr_0008', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'completed', 1789913600000, 0, 5, 2, 7);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, tool_call_count, input_tokens, output_tokens, computed_total_tokens)
VALUES ('mu_0009', 'lr_0009', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'completed', 1789913599999, 0, 6, 5, 11);
-- 窗口外的旧行（25 h / 30 h 前）：不计入。
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, tool_call_count, input_tokens, output_tokens, computed_total_tokens)
VALUES ('mu_0010', 'lr_0010', 'sess_a0000000-0000-4000-8000-000000000001', 'main_turn', 'zai', 'glm-fixture', 'zcode-agent', 'yolo', 'interactive', 'completed', 1789910000000, 9, 990000, 9999, 999999);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, tool_call_count, input_tokens, output_tokens, computed_total_tokens)
VALUES ('mu_0011', 'lr_0011', 'sess_subagent_agent_10000000-0000-4000-8000-000000000004', 'subagent', 'zai', 'glm-fixture', 'zcode-Explore', 'yolo', 'subagent_child', 'completed', 1789892000000, 7, 550000, 5555, 555555);

-- 子 agent：query_source = subagent 或 task_type = subagent_child 任一成立即算。
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0012', 'lr_0012', 'sess_subagent_agent_10000000-0000-4000-8000-000000000001', 'subagent', 'zai', 'glm-fixture', 'zcode-Explore', 'yolo', 'subagent_child', 'completed', 1789992801000, 1789992830000, 5, 8000, 600, 0, 6000, 8600, 8600);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0013', 'lr_0013', 'sess_subagent_agent_10000000-0000-4000-8000-000000000001', 'subagent', 'zai', 'glm-fixture', 'zcode-Explore', 'yolo', 'subagent_child', 'completed', 1789993400000, 1789993430000, 4, 9000, 700, 0, 7000, 9700, 9700);
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0014', 'lr_0014', 'sess_subagent_agent_10000000-0000-4000-8000-000000000002', 'subagent', 'zai', 'glm-fixture', 'zcode-general-purpose', 'yolo', 'subagent_child', 'completed', 1789997000000, 1789997030000, 2, 4000, 300, 0, 2000, NULL, 4300);
-- task_type 缺失，但 query_source 说明是子 agent。
INSERT INTO model_usage (id, logical_request_id, session_id, query_source, provider_id, model_id, agent, mode, task_type, status, started_at, completed_at, tool_call_count, input_tokens, output_tokens, reasoning_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
VALUES ('mu_0015', 'lr_0015', 'sess_subagent_agent_10000000-0000-4000-8000-000000000003', 'subagent', 'zai', 'glm-fixture', 'zcode-vision', 'yolo', NULL, 'completed', 1789998800000, 1789998830000, 1, 1500, 100, 0, 0, 1600, 1600);
