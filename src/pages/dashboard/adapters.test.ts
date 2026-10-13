import { describe, expect, it } from 'vitest';
import {
  wbOfficialAllToPoints,
  wbOfficialModels,
  wbRegionAvailable,
  type WbRegionFilter,
} from './adapters';
import type { WbUsageOfficialAll } from '../../types';

/**
 * 回归：积分统计（Buddy 官网源）版本切换的**不串版**契约。
 *
 * 缺陷背景：旧适配器用 `daily_by_region?.[region] ?? daily`，当所选版本在后端分桶中
 * 缺席时静默回落到**全量（合并）**数据——用户切到「国际版」看到的却是国内版的数字。
 * 后端只为实际遍历到的账号所属版本建桶，故「国际版桶缺席」是真实可达的状态。
 */

/** 构造 v2 结构：仅含传入版本的分桶（模拟后端 region_aggs 的建桶语义） */
function official(
  byRegion: Record<string, { usage: number; model?: string; credit?: number }>,
): WbUsageOfficialAll {
  const daily: { date: string; usage: number }[] = [{ date: '2026-01-10', usage: 999 }];
  const daily_by_region: Record<string, { date: string; usage: number }[]> = {};
  const models_by_region: Record<string, { model: string; request_count: number; credit: number }[]> = {};
  const summary_by_region: Record<string, any> = {};
  for (const [rk, v] of Object.entries(byRegion)) {
    daily_by_region[rk] = [{ date: '2026-01-10', usage: v.usage }];
    models_by_region[rk] = [{ model: v.model ?? 'm', request_count: 1, credit: v.credit ?? v.usage }];
    summary_by_region[rk] = { accounts_ok: 1, accounts_total: 1, request_count_total: 1 };
  }
  return {
    status: 'complete',
    source: 'official_all',
    schema_version: 2,
    accounts_total: 1,
    accounts_ok: 1,
    range_start: '2026-01-10',
    range_end: '2026-01-10',
    fetched_at_ms: 0,
    request_count_total: 1,
    summary: { usage_today: 0, usage_7days: 0, usage_this_month: 0 },
    daily,
    daily_by_region,
    models_by_region,
    summary_by_region,
  };
}

describe('wbOfficialAllToPoints 版本隔离', () => {
  it('国际版桶缺席时不回落全量（不拿国内版数据冒充国际版）', () => {
    const r = official({ cn: { usage: 999 } });
    expect(wbOfficialAllToPoints(r, 'global')).toEqual([]);
  });

  it('国内版桶缺席时同样不回落（对称保证）', () => {
    const r = official({ global: { usage: 123 } });
    expect(wbOfficialAllToPoints(r, 'cn')).toEqual([]);
  });

  it('所选版本存在时精确取该版本序列', () => {
    const r = official({ cn: { usage: 999 }, global: { usage: 123 } });
    expect(wbOfficialAllToPoints(r, 'global').map((p) => p.credits)).toEqual([123]);
    expect(wbOfficialAllToPoints(r, 'cn').map((p) => p.credits)).toEqual([999]);
  });

  it('合并视图仍取全量序列', () => {
    const r = official({ cn: { usage: 999 } });
    expect(wbOfficialAllToPoints(r, 'all').map((p) => p.credits)).toEqual([999]);
  });

  it('null 与旧结构缓存（无分桶）走安全路径', () => {
    expect(wbOfficialAllToPoints(null, 'global')).toEqual([]);
    const legacy = { ...official({ cn: { usage: 1 } }), daily_by_region: undefined };
    // 旧缓存无分桶：'all' 用全量；指定版本时无桶可查 → 空（由页面按 regionAvailable 回落 'all'）
    expect(wbOfficialAllToPoints(legacy as WbUsageOfficialAll, 'all').length).toBe(1);
    expect(wbOfficialAllToPoints(legacy as WbUsageOfficialAll, 'global')).toEqual([]);
  });
});

describe('wbOfficialModels 版本隔离', () => {
  it('国际版桶缺席时返回空模型列表，不回落全量', () => {
    const r = official({ cn: { usage: 10, model: 'glm-5.2' } });
    expect(wbOfficialModels(r, 'global')).toEqual([]);
    expect(wbOfficialModels(r, 'cn').map((m) => m.model)).toEqual(['glm-5.2']);
  });

  it('合并视图取全量模型汇总', () => {
    const r = official({ global: { usage: 10, model: 'claude-sonnet' } });
    r.models = [{ model: 'tot', request_count: 1, credit: 5 }];
    expect(wbOfficialModels(r, 'all').map((m) => m.model)).toEqual(['tot']);
  });
});

describe('wbRegionAvailable 版本存在性', () => {
  it('如实区分「该版本为 0」与「无该版本」', () => {
    const r = official({ cn: { usage: 0 } });
    expect(wbRegionAvailable(r, 'cn')).toBe(true); // 存在桶但值为 0
    expect(wbRegionAvailable(r, 'global')).toBe(false); // 桶缺席
    expect(wbRegionAvailable(r, 'all')).toBe(true);
    expect(wbRegionAvailable(null, 'cn')).toBe(false);
  });

  it('分桶键为原型链属性时不误判（hasOwnProperty 语义）', () => {
    const r = official({ cn: { usage: 1 } });
    (r as any).daily_by_region = Object.create({ global: [] });
    const region: WbRegionFilter = 'global';
    expect(wbRegionAvailable(r, region)).toBe(false);
  });
});
