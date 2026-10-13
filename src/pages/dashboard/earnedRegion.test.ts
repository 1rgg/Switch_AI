import { describe, expect, it } from 'vitest';
import { wbEarnedByDateFiltered } from './adapters';
import type { WbCheckinRecord, WbCreditsSnapshot, WbUsageOfficialAll } from '../../types';

/**
 * 回归：Buddy「获得积分」必须随版本选择变化。
 *
 * 缺陷背景：消耗侧按版本分桶，获得侧（快照 earned / 签到 reward）此前**完全无视版本**——
 * 切国内版/国际版时「区间总获得」一模一样（用户截图即为此现象），是明显的口径错误。
 * 修复方式：经官方用量的账号清单（accounts[].region）反查快照/签到记录 user_id 的版本，
 * 按与消耗同一版本口径收敛。
 *
 * 本用例验证「口径收敛的推导链」成立：同一批快照/签到数据，按不同 uid 集过滤后
 * 得到不同的 earned 结果（即按版本可区分），而不是恒定同一个数。
 */

/** 与 CreditsTab.regionUids 同款推导：所选版本的 uid 集 */
function regionUids(official: WbUsageOfficialAll, region: string): Set<string> {
  return new Set((official.accounts ?? []).filter((a) => a.region === region).map((a) => a.account_id));
}

const snapshots: WbCreditsSnapshot[] = [
  {
    date: '2026-10-12',
    ts: 1,
    total_balance: 0,
    earned: 300,
    accounts: [
      { user_id: 'cn-1', balance: 100 },
      { user_id: 'gl-1', balance: 200 },
    ],
  },
  {
    date: '2026-10-13',
    ts: 2,
    total_balance: 0,
    earned: 777,
    accounts: [
      { user_id: 'cn-1', balance: 400 },
      { user_id: 'gl-1', balance: 677 },
    ],
  },
];

const checkin: WbCheckinRecord[] = [
  { date: '2026-10-13', time: '', user_id: 'cn-1', name: 'cn', status: 'ok', message: '', reward: 100 },
  { date: '2026-10-13', time: '', user_id: 'gl-1', name: 'gl', status: 'ok', message: '', reward: 50 },
];

const official = {
  accounts: [
    { account_id: 'cn-1', region: 'cn' },
    { account_id: 'gl-1', region: 'global' },
  ],
} as unknown as WbUsageOfficialAll;

describe('获得积分按版本收敛', () => {
  it('不同版本得到不同的 earned（不再恒定同一个数）', () => {
    const sum = (uids: Set<string>) => {
      const m = wbEarnedByDateFiltered(snapshots, checkin, uids);
      return [...m.values()].reduce((a, b) => a + b, 0);
    };
    const cnTotal = sum(regionUids(official, 'cn'));
    const glTotal = sum(regionUids(official, 'global'));
    expect(cnTotal).toBeGreaterThan(0);
    expect(glTotal).toBeGreaterThan(0);
    // 这是缺陷的核心断言：两个版本的获得积分必须不同
    expect(cnTotal).not.toBe(glTotal);
  });

  it('全量（合并视图）等价于两版本之和', () => {
    const sum = (uids: Set<string>) =>
      [...wbEarnedByDateFiltered(snapshots, checkin, uids).values()].reduce((a, b) => a + b, 0);
    const cnTotal = sum(regionUids(official, 'cn'));
    const glTotal = sum(regionUids(official, 'global'));
    const allTotal = sum(new Set(['cn-1', 'gl-1']));
    // 逐账号差分在同一行内取 max 后相加，故「分版本求和 == 全量」成立——
    // 这正是本次修复的正确性依据：分版本不丢数、不重复计数
    expect(cnTotal + glTotal).toBe(allTotal);
  });

  it('空 uid 集（该版本无账号）得 0，而非回落到全量', () => {
    const m = wbEarnedByDateFiltered(snapshots, checkin, new Set<string>());
    expect(m.size).toBe(0);
  });

  it('国际版账号的获得只来自国际版账号的余额差分/签到', () => {
    const gl = wbEarnedByDateFiltered(snapshots, checkin, regionUids(official, 'global'));
    // 10-13 国际版：余额差分 677-200=477，签到 50 → max(477,50)=477
    expect(gl.get('2026-10-13')).toBe(477);
    // 关键：不得混入国内账号的差分（cn-1 为 300）；串版本会得到 777
    expect(gl.get('2026-10-13')).not.toBe(777);
  });
});
