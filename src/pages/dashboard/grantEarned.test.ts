import { describe, expect, it } from 'vitest';
import { wbGrantEarnedByDate } from './adapters';
import type { WbCreditAccount, WbCreditPackage } from '../../types';

/**
 * 回归：Buddy「获得积分」权威口径 = 积分包发放日归集。
 *
 * fixture 取自国际站 `get-user-resource` 的**真实响应**（脱敏：仅保留断言所需字段）。
 * 该口径取代「余额差分推导」——后者会被包过期/消耗波动污染，且差分守卫会漏算。
 */

function pkg(p: Partial<WbCreditPackage>): WbCreditPackage {
  return { name: '', remaining: 0, total: 0, used: 0, end_time: null, expire_ts: null, ...p };
}

function acc(user_id: string, packages: WbCreditPackage[]): WbCreditAccount {
  return { user_id, name: user_id, ok: true, balance: 0, packages, source: 'cloud' };
}

/** 真实样本：Bonus Pack 250(10-07) / Free Plan 100(10-01) / Bonus 30 ×3 */
const realAccounts: WbCreditAccount[] = [
  acc('u1', [
    pkg({ name: 'Bonus Pack', grant_date: '2026-10-07', grant_amount: 250 }),
    pkg({ name: 'Free Plan Subscription', grant_date: '2026-10-01', grant_amount: 100 }),
    pkg({ name: 'Bonus Pack', grant_date: '2026-10-08', grant_amount: 30 }),
    pkg({ name: 'Bonus Pack', grant_date: '2026-10-10', grant_amount: 30 }),
    pkg({ name: 'Bonus Pack', grant_date: '2026-10-11', grant_amount: 30 }),
    pkg({ name: 'Bonus Pack', grant_date: '2026-10-13', grant_amount: 30 }),
  ]),
];

describe('wbGrantEarnedByDate（积分包发放日归集）', () => {
  it('按发放日归集面额（真实样本）', () => {
    const m = wbGrantEarnedByDate(realAccounts);
    expect(m.get('2026-10-01')).toBe(100);
    expect(m.get('2026-10-07')).toBe(250);
    expect(m.get('2026-10-08')).toBe(30);
    expect(m.get('2026-10-10')).toBe(30);
    expect(m.get('2026-10-11')).toBe(30);
    expect(m.get('2026-10-13')).toBe(30);
    // 总量守恒
    expect([...m.values()].reduce((a, b) => a + b, 0)).toBe(470);
  });

  it('跨账号同日发放累加（多账号池）', () => {
    const two = [
      acc('u1', [pkg({ grant_date: '2026-10-13', grant_amount: 30 })]),
      acc('u2', [pkg({ grant_date: '2026-10-13', grant_amount: 50 })]),
    ];
    expect(wbGrantEarnedByDate(two).get('2026-10-13')).toBe(80);
  });

  it('缺 grant_date 或金额非正的包被跳过（不臆造、不计 0 条目）', () => {
    const m = wbGrantEarnedByDate([
      acc('u1', [
        pkg({ grant_date: null, grant_amount: 100 }), // 无发放时刻
        pkg({ grant_date: '2026-10-13', grant_amount: 0 }), // 面额 0
        pkg({ grant_date: '2026-10-13', grant_amount: -5 }), // 脏数据
        pkg({ grant_date: '2026-10-14', grant_amount: 20 }), // 唯一有效
      ]),
    ]);
    expect([...m.entries()]).toEqual([['2026-10-14', 20]]);
  });

  it('空输入安全（账号为空 / 包为空 / null）', () => {
    expect(wbGrantEarnedByDate(null).size).toBe(0);
    expect(wbGrantEarnedByDate([]).size).toBe(0);
    expect(wbGrantEarnedByDate([acc('u1', [])]).size).toBe(0);
  });

  it('同一版本的多个账号只经 uid 过滤后归集（版本收敛依据）', () => {
    const pool = [
      acc('cn-1', [pkg({ grant_date: '2026-10-13', grant_amount: 100 })]),
      acc('gl-1', [pkg({ grant_date: '2026-10-13', grant_amount: 30 })]),
    ];
    const globalUids = new Set(['gl-1']);
    const gl = wbGrantEarnedByDate(pool.filter((a) => globalUids.has(a.user_id)));
    const cn = wbGrantEarnedByDate(pool.filter((a) => a.user_id === 'cn-1'));
    // 版本隔离：两侧数字不同，且合计等于全量（不丢数、不重复）
    expect(gl.get('2026-10-13')).toBe(30);
    expect(cn.get('2026-10-13')).toBe(100);
    expect(wbGrantEarnedByDate(pool).get('2026-10-13')).toBe(130);
  });
});
