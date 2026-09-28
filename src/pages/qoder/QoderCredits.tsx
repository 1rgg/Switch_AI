import { useCallback, useEffect, useState } from 'react';
import { Coins, RefreshCw } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { QoderCreditsResult, QoderCreditsSnapshot } from '../../types';

/**
 * qoder-credits 积分看板（F-80 §5.8，M1 版）：
 * KPI 卡（总余额/Plan/Add-on）+ 各账号积分包明细 + 近 7 日余额趋势。
 * M1 数据源：token 直调 usage（pat / client_token 徽标）；jobToken 交换与 CLI
 * 解析通道随 M2 接入；产品线（lingma_）识别横幅随 R-7 闭合后补充。
 */

export default function QoderCredits() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [credits, setCredits] = useState<QoderCreditsResult | null>(null);
  const [snapshots, setSnapshots] = useState<QoderCreditsSnapshot[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async (fresh = false) => {
    if (fresh) setRefreshing(true);
    else setLoading(true);
    try {
      // I18：creditsFetch 不再内联吞错（原 .catch(() => null) 使外层 catch 成死代码、
      // 失败静默呈空态）；historyList 失败仍降级为空序列（不阻塞主数据展示）
      const [c, h] = await Promise.all([
        api.qoder.creditsFetch(undefined, fresh),
        api.qoder.creditsHistoryList().catch(() => ({ snapshots: [] as QoderCreditsSnapshot[] })),
      ]);
      setCredits(c);
      setSnapshots(h.snapshots ?? []);
    } catch (err) {
      setCredits(null);
      pushToast('error', `读取积分失败：${String(err)}`);
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh(false);
  }, [refresh]);

  const planSum = credits?.accounts.reduce((s, a) => s + (a.plan_credits ?? 0), 0) ?? 0;
  const addonSum = credits?.accounts.reduce((s, a) => s + (a.addon_credits ?? 0), 0) ?? 0;
  const recent = snapshots.slice(-7);
  const maxBalance = Math.max(1, ...recent.map((s) => s.total_balance));

  return (
    <div className="animate-fade-in">
      <PageHeader title="Qoder · 积分看板" desc="余额 / 积分包 / 趋势 · 官方 PAT 通道优先" />

      {credits?.stale && (
        <div className="mb-4 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
          {credits.stale_reason || '本轮刷新失败，展示历史缓存数据'}
        </div>
      )}

      {/* KPI 卡 */}
      <div className="grid gap-4 md:grid-cols-3">
        <StatCard
          label="可用总积分"
          value={credits?.total_balance != null ? credits.total_balance.toLocaleString() : '—'}
          hint={credits?.cached ? '缓存数据' : undefined}
          tone="blue"
        />
        {/* I22：0 是合法余额（积分用光），原 truthiness 判断把 0 显示成「—」 */}
        <StatCard label="Plan Credits" value={credits ? planSum.toLocaleString() : '—'} hint="按购买日周期重置归零" />
        <StatCard label="Add-on Credits" value={credits ? addonSum.toLocaleString() : '—'} hint="签到/奖励所得 · 独立有效期" tone="green" />
      </div>

      {/* 各账号明细 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Coins size={16} className="text-violet-500" />
            <span className="text-sm font-medium">账号明细</span>
            {credits?.cached && !credits?.stale && <Badge tone="slate">缓存</Badge>}
            {credits?.accounts.map((a) =>
              a.source === 'pat' ? <Badge key={a.user_id} tone="green">PAT 通道</Badge> : a.source === 'client_token' ? <Badge key={a.user_id} tone="blue">客户端凭证</Badge> : null,
            )}
          </div>
          <button className="btn-outline !px-3 !py-1 text-xs" disabled={refreshing} onClick={() => void refresh(true)}>
            <RefreshCw size={13} className={refreshing ? 'animate-spin' : ''} /> 强制刷新
          </button>
        </div>
        <div className="rounded-lg border border-slate-200 dark:border-zinc-700">
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-3 py-1.5 text-left">账号</th>
                <th className="px-3 py-1.5 text-right">Plan</th>
                <th className="px-3 py-1.5 text-right">Add-on</th>
                <th className="px-3 py-1.5 text-right">合计</th>
                <th className="px-3 py-1.5 text-left">积分包 / 到期</th>
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <tr><td colSpan={5} className="px-3 py-4 text-center text-xs text-slate-400">加载中…</td></tr>
              ) : !credits || credits.accounts.length === 0 ? (
                <tr>
                  <td colSpan={5} className="px-3 py-4 text-center text-xs text-slate-400">
                    {credits?.message || '暂无账号：请先在「账号管理」导入 PAT'}
                  </td>
                </tr>
              ) : (
                credits.accounts.map((a) => (
                  <tr key={a.user_id} className="border-t border-slate-100 dark:border-zinc-800">
                    <td className="px-3 py-1.5">
                      <div className="font-medium">{a.name}</div>
                      <div className="text-xs text-slate-400">{a.ok ? '' : a.message || '查询失败'}</div>
                    </td>
                    <td className="px-3 py-1.5 text-right tabular-nums text-xs">{a.plan_credits ?? '—'}</td>
                    <td className="px-3 py-1.5 text-right tabular-nums text-xs">{a.addon_credits ?? '—'}</td>
                    <td className="px-3 py-1.5 text-right tabular-nums text-xs font-medium">{a.total ?? '—'}</td>
                    <td className="px-3 py-1.5 text-xs text-slate-500">
                      {a.packages.length === 0 ? (
                        <span className="text-slate-300 dark:text-zinc-600">—</span>
                      ) : (
                        a.packages.map((p, i) => (
                          <div key={i}>
                            {p.amount ?? '?'}
                            {p.expire_at ? ` · 到期 ${p.expire_at}` : ''}
                            {p.source ? `（${p.source}）` : ''}
                          </div>
                        ))
                      )}
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
        <p className="mt-3 text-xs text-slate-400">
          扣减规则：FEFO（最先到期优先）· 同到期先 Plan 后 Add-on · 失败请求不扣费。
          响应结构 R-7 固化前为宽容解析，字段缺失时以「—」展示。
        </p>
      </div>

      {/* 近 7 日余额趋势 */}
      {recent.length > 1 && (
        <div className="mt-4 card p-4">
          <div className="mb-3 text-sm font-medium">近 {recent.length} 日余额快照</div>
          <div className="flex h-32 items-end gap-2">
            {recent.map((s) => (
              <div key={s.date} className="flex flex-1 flex-col items-center gap-1">
                <div
                  className="w-full rounded-t bg-violet-500/70 dark:bg-violet-400/60"
                  style={{ height: `${Math.max(4, (s.total_balance / maxBalance) * 100)}%` }}
                  title={`${s.date}: ${s.total_balance}`}
                />
                <span className="text-[10px] text-slate-400">{s.date.slice(5)}</span>
              </div>
            ))}
          </div>
          <p className="mt-2 text-xs text-slate-400">
            快照每日由应用内调度器（默认 23:40）与打开本页时写入，未开机日子会缺天。
          </p>
        </div>
      )}
    </div>
  );
}
