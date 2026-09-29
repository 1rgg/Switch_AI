import { useCallback, useEffect, useMemo, useState } from 'react';
import { Bar, Line, XAxis, YAxis, ComposedChart, ResponsiveContainer, Tooltip, CartesianGrid } from 'recharts';
import { Coins, RefreshCw } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import ActivityHeatmap from '../../components/charts/ActivityHeatmap';
import { Badge, EmptyState, StatCard } from '../../components/ui';
import { RANGES, useDateRange } from '../../hooks/useDateRange';
import { useIsDark } from '../../lib/useIsDark';
import { fmtCredits } from '../../lib/format';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { QoderCreditsResult, QoderCreditsSnapshot } from '../../types';

/**
 * qoder-credits 积分看板 v2（F-80 §5.8 重构，设计对齐 Buddy 积分看板 CreditsTab）：
 * ① 订阅版本的资源（Plan 剩余/已用进度）② 个人资源包（Add-on + 积分包）
 * ③ Credits 消耗热力图 ④ Credits 消耗（区间趋势：消耗柱 + 余额/获得线）⑤ Credits 记录（账号明细）。
 * 消耗口径（页面内诚实标注）：Qoder 无官方日消耗端点，采用快照差分 ——
 * 消耗 = 前日余额 − 当日余额 + 当日签到奖励（负值记 0，首日 null）；获得 = 当日签到奖励合计。
 */

// 图表 tooltip 样式（对齐 CreditsTab 同名辅助）
const tooltipStyle = (isDark: boolean) => ({
  fontSize: 12,
  borderRadius: 10,
  border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
  background: isDark ? '#18181b' : '#fff',
  color: isDark ? '#e4e4e7' : '#1e293b',
  boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
  padding: '8px 12px',
} as const);

/** 配额进度条（used% 单色填充，超界钳制） */
function ProgressBar({ pct, color }: { pct: number; color: string }) {
  return (
    <div className="h-1.5 w-full overflow-hidden rounded-full bg-slate-100 dark:bg-zinc-800">
      <div
        className="h-full rounded-full transition-all"
        style={{ width: `${Math.max(0, Math.min(100, pct))}%`, background: color }}
      />
    </div>
  );
}

export default function QoderCredits() {
  const pushToast = useAppStore((s) => s.pushToast);
  const isDark = useIsDark();
  const { range, setRange, startStr, todayStr, dateList } = useDateRange('30d');
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
      // 失败保留已有数据（原 setCredits(null) 把可用的历史缓存清掉呈空态）
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

  // ---- ① 订阅版本的资源：ok 且含 Plan 余额的账号 ----
  const okAccounts = useMemo(
    () => credits?.accounts.filter((a) => a.ok && a.plan_credits != null) ?? [],
    [credits],
  );
  const planAgg = useMemo(() => {
    const remaining = okAccounts.reduce((s, a) => s + (a.plan_credits ?? 0), 0);
    const usedKnown = okAccounts.some((a) => a.plan_used != null);
    const used = okAccounts.reduce((s, a) => s + (a.plan_used ?? 0), 0);
    const total = usedKnown ? used + remaining : null;
    const pct = total && total > 0 ? (used / total) * 100 : 0;
    // 最早订阅到期（YYYY-MM-DD 字符串序即时间序）
    const expiry = okAccounts.map((a) => a.plan_expires_at).filter(Boolean).sort()[0] ?? '';
    return { remaining, used, usedKnown, total, pct, expiry };
  }, [okAccounts]);

  // ---- ② 个人资源包：ok 且含 Add-on 余额或积分包的账号 ----
  const addonAccounts = useMemo(
    () => credits?.accounts.filter((a) => a.ok && (a.addon_credits != null || a.packages.length > 0)) ?? [],
    [credits],
  );
  const addonAgg = useMemo(() => {
    const remaining = addonAccounts.reduce((s, a) => s + (a.addon_credits ?? 0), 0);
    const usedKnown = addonAccounts.some((a) => a.addon_used != null);
    const used = addonAccounts.reduce((s, a) => s + (a.addon_used ?? 0), 0);
    const total = usedKnown ? used + remaining : null;
    const pct = total && total > 0 ? (used / total) * 100 : 0;
    return { remaining, used, usedKnown, total, pct };
  }, [addonAccounts]);

  // ---- ③ 热力图：全量历史消耗（快照差分，consumed != null 的天） ----
  const heatValues = useMemo(() => {
    const m = new Map<string, number>();
    for (const s of snapshots) {
      if (s.consumed != null) m.set(s.date, (m.get(s.date) ?? 0) + s.consumed);
    }
    return m;
  }, [snapshots]);

  // ---- ④ 区间消耗：区间聚合 + 趋势（dateList 铺底，缺天为 null） ----
  const snapByDate = useMemo(() => new Map(snapshots.map((s) => [s.date, s])), [snapshots]);
  // 本地自然日 ISO 字符串可直接比较；作为 memo 依赖以 startStr/todayStr 表达
  const inRange = (date: string) => date >= startStr && date <= todayStr;
  const rangeAgg = useMemo(() => {
    let consumed = 0;
    let earned = 0;
    for (const s of snapshots) {
      if (!inRange(s.date)) continue;
      if (s.consumed != null) consumed += s.consumed;
      if (s.earned != null) earned += s.earned;
    }
    return { consumed, earned };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [snapshots, startStr, todayStr]);
  const trend = useMemo(
    () =>
      dateList.map((date) => {
        const s = snapByDate.get(date);
        return {
          label:
            range === 'year'
              ? `${+date.slice(0, 4)}/${+date.slice(5, 7)}/${+date.slice(8, 10)}`
              : `${+date.slice(5, 7)}/${+date.slice(8, 10)}`,
          consumed: s?.consumed ?? null,
          balance: s?.total_balance ?? null,
          earned: s?.earned ?? null,
        };
      }),
    [dateList, range, snapByDate],
  );
  const hasTrend = trend.some((d) => d.consumed != null || d.balance != null || d.earned != null);
  const showDots = range === 'today' || range === '7d';

  // ---- ⑤ 记录表：通道徽标按 source 去重 ----
  const channels = useMemo(() => [...new Set(credits?.accounts.map((a) => a.source) ?? [])], [credits]);

  const axisProps = {
    tick: { fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' },
    axisLine: { stroke: isDark ? '#3f3f46' : '#e2e8f0' },
    tickLine: false,
  } as const;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 积分看板"
        desc="订阅资源 / 资源包 / 消耗趋势 / 记录 · 官方 PAT 通道优先"
        actions={
          <button className="btn-outline" disabled={refreshing} onClick={() => void refresh(true)}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 强制刷新
          </button>
        }
      />

      {credits?.stale && (
        <div className="mb-4 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
          {credits.stale_reason || '本轮刷新失败，展示历史缓存数据'}
        </div>
      )}

      <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
        {/* ① 订阅版本的资源 */}
        <div className="card p-5">
          <div className="mb-3 flex items-center gap-2">
            <h4 className="text-sm font-medium">订阅版本的资源</h4>
            <span className="text-xs text-slate-400">Plan Credits · 按购买日周期重置归零</span>
          </div>
          {loading ? (
            <EmptyState icon={<Coins size={22} />} title="加载中…" hint="正在读取账号积分。" />
          ) : okAccounts.length === 0 ? (
            <EmptyState
              icon={<Coins size={22} />}
              title="暂无 Plan 数据"
              hint="请先在「账号管理」导入 PAT，或强制刷新后重试。"
            />
          ) : (
            <>
              <div className="flex items-baseline justify-between gap-2">
                <div className="flex items-baseline gap-2">
                  <span className="text-2xl font-semibold tabular-nums">{fmtCredits(planAgg.remaining)}</span>
                  <span className="text-xs text-slate-400">Plan 剩余</span>
                </div>
                {planAgg.total != null && (
                  <span className="text-xs tabular-nums text-slate-400">
                    已用 {fmtCredits(planAgg.used)} / 共 {fmtCredits(planAgg.total)}
                  </span>
                )}
              </div>
              {planAgg.total != null && (
                <div className="mt-2">
                  <ProgressBar pct={planAgg.pct} color="#f59e0b" />
                </div>
              )}
              {planAgg.expiry && (
                <div className="mt-1.5 text-[11px] text-slate-400">最早订阅到期 {planAgg.expiry}</div>
              )}
              <div className="mt-4 space-y-2">
                {okAccounts.map((a) => {
                  const pair = a.plan_used != null && a.plan_credits != null;
                  const used = a.plan_used ?? 0;
                  const total = (a.plan_used ?? 0) + (a.plan_credits ?? 0);
                  const pct = total > 0 ? (used / total) * 100 : 0;
                  return (
                    <div key={a.user_id} className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
                      <div className="flex items-center justify-between gap-2 text-xs">
                        <span className="font-medium">{a.name}</span>
                        <span className="text-slate-400">{a.plan_expires_at ? `到期 ${a.plan_expires_at}` : '到期未知'}</span>
                      </div>
                      {pair ? (
                        <>
                          <div className="mt-2">
                            <ProgressBar pct={pct} color="#f59e0b" />
                          </div>
                          <div className="mt-1.5 flex justify-between text-[11px] tabular-nums text-slate-400">
                            <span>已用 {fmtCredits(used)}</span>
                            <span>共 {fmtCredits(total)}</span>
                            <span>剩余 {fmtCredits(a.plan_credits ?? 0)}</span>
                          </div>
                        </>
                      ) : (
                        <div className="mt-2 text-[11px] text-slate-400">
                          剩余 {fmtCredits(a.plan_credits ?? 0)} · 用量未采集（刷新后可得）
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </>
          )}
        </div>

        {/* ② 个人资源包 */}
        <div className="card p-5">
          <div className="mb-3 flex items-center gap-2">
            <h4 className="text-sm font-medium">个人资源包</h4>
            <span className="text-xs text-slate-400">Add-on Credits · 签到/奖励所得 · 独立有效期</span>
          </div>
          {loading ? (
            <EmptyState icon={<Coins size={22} />} title="加载中…" hint="正在读取资源包数据。" />
          ) : addonAccounts.length === 0 ? (
            <EmptyState
              icon={<Coins size={22} />}
              title="暂无资源包"
              hint="签到/奖励所得或购买的积分包将在此展示。"
            />
          ) : (
            <>
              <div className="flex items-baseline justify-between gap-2">
                <div className="flex items-baseline gap-2">
                  <span className="text-2xl font-semibold tabular-nums">{fmtCredits(addonAgg.remaining)}</span>
                  <span className="text-xs text-slate-400">Add-on 剩余</span>
                </div>
                {addonAgg.total != null && (
                  <span className="text-xs tabular-nums text-slate-400">
                    已用 {fmtCredits(addonAgg.used)} / 共 {fmtCredits(addonAgg.total)}
                  </span>
                )}
              </div>
              {addonAgg.total != null && (
                <div className="mt-2">
                  <ProgressBar pct={addonAgg.pct} color="#22c55e" />
                </div>
              )}
              <div className="mt-4 space-y-2">
                {addonAccounts
                  .filter((a) => a.packages.length > 0 || a.addon_credits != null)
                  .map((a) => (
                    <div key={a.user_id} className="rounded-lg border border-slate-200 p-3 dark:border-zinc-700">
                      <div className="text-xs font-medium">{a.name}</div>
                      {a.addon_credits != null && (
                        <div className="mt-1 text-[11px] tabular-nums text-slate-400">
                          Add-on 剩余 {fmtCredits(a.addon_credits)}
                          {a.addon_used != null ? ` · 已用 ${fmtCredits(a.addon_used)}` : ''}
                        </div>
                      )}
                      {a.packages.map((p, i) => (
                        <div key={i} className="mt-0.5 text-[11px] text-slate-500">
                          积分包 {p.amount ?? '?'}
                          {p.expire_at ? ` · 到期 ${p.expire_at}` : ''}
                          {p.source ? `（${p.source}）` : ''}
                        </div>
                      ))}
                    </div>
                  ))}
              </div>
            </>
          )}
        </div>
      </div>

      {/* ③ Credits 消耗热力图 */}
      <div className="card mt-4 p-5">
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <h4 className="text-sm font-medium">Credits 消耗热力图</h4>
          <span className="text-xs text-slate-400">消耗为快照差分推导 · 全量历史</span>
        </div>
        <ActivityHeatmap
          values={heatValues}
          unit="积分"
          fmtValue={fmtCredits}
          emptyHint="暂无消耗记录：快照每日由应用内调度器（默认 23:40）与打开本页时写入，次日即可查看。"
        />
      </div>

      {/* ④ Credits 消耗（区间趋势） */}
      <div className="card mt-4 p-5">
        <div className="mb-2 flex flex-wrap items-center gap-3">
          <h4 className="text-sm font-medium">Credits 消耗</h4>
          <div className="flex items-center gap-3 text-xs text-slate-400">
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#f59e0b' }} />
              消耗
            </span>
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#8b5cf6' }} />
              余额
            </span>
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#22c55e' }} />
              获得
            </span>
          </div>
          <span className="text-xs text-slate-400">
            消耗/获得为快照差分推导 · 快照每日 23:40 由调度器写入 · 未开机缺天
          </span>
          <div className="ml-auto flex items-center gap-1">
            {RANGES.map((r) => (
              <button
                key={r.key}
                onClick={() => setRange(r.key)}
                className={`chip border ${
                  range === r.key
                    ? 'border-brand-500 text-brand-600 dark:text-brand-400'
                    : 'border-slate-200 text-slate-500 dark:border-zinc-700 dark:text-zinc-400'
                }`}
              >
                {r.label}
              </button>
            ))}
          </div>
        </div>
        <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
          <StatCard label="区间总消耗" value={fmtCredits(rangeAgg.consumed)} hint="前日余额 − 当日余额 + 当日签到奖励" tone="amber" />
          <StatCard label="区间总获得" value={fmtCredits(rangeAgg.earned)} hint="当日签到奖励合计" tone="green" />
          <StatCard
            label="当前余额"
            value={credits?.total_balance != null ? fmtCredits(credits.total_balance) : '—'}
            hint={credits?.cached ? '缓存数据' : undefined}
            tone="blue"
          />
        </div>
        <div className="mt-4">
          {!hasTrend ? (
            <EmptyState
              icon={<Coins size={22} />}
              title="暂无趋势数据"
              hint="快照每日由应用内调度器（默认 23:40）与打开本页时写入，次日即可查看趋势。"
            />
          ) : (
            <div className="h-56">
              <ResponsiveContainer>
                <ComposedChart data={trend} margin={{ top: 12, right: 16, left: 0, bottom: 4 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                  <XAxis dataKey="label" {...axisProps} minTickGap={24} />
                  <YAxis {...axisProps} axisLine={false} width={56} />
                  <Tooltip
                    cursor={{ stroke: isDark ? '#52525b' : '#cbd5e1', strokeWidth: 1, strokeDasharray: '3 3' }}
                    contentStyle={tooltipStyle(isDark)}
                    // 缺天数据为 null：fmtCredits 期望 number，null/非数值直接显示 —（参数类型须兼容 recharts ValueType）
                    formatter={(v: number | string | (number | string)[] | null | undefined, name: string) => [
                      typeof v === 'number' && isFinite(v) ? fmtCredits(v) : '—',
                      name,
                    ]}
                  />
                  <Bar dataKey="consumed" name="消耗" fill="#f59e0b" maxBarSize={22} radius={[3, 3, 0, 0]} />
                  <Line
                    type="monotone"
                    dataKey="balance"
                    name="余额"
                    stroke="#8b5cf6"
                    strokeWidth={2}
                    dot={showDots ? { r: 3, fill: '#8b5cf6', strokeWidth: 0 } : false}
                    activeDot={{ r: 5 }}
                    connectNulls
                  />
                  <Line
                    type="monotone"
                    dataKey="earned"
                    name="获得"
                    stroke="#22c55e"
                    strokeWidth={2}
                    dot={showDots ? { r: 3, fill: '#22c55e', strokeWidth: 0 } : false}
                    activeDot={{ r: 5 }}
                    connectNulls
                  />
                </ComposedChart>
              </ResponsiveContainer>
            </div>
          )}
        </div>
      </div>

      {/* ⑤ Credits 记录（账号明细） */}
      <div className="card mt-4 p-5">
        <div className="mb-2 flex flex-wrap items-center gap-2">
          <h4 className="text-sm font-medium">Credits 记录</h4>
          <span className="text-xs text-slate-400">
            扣减规则：FEFO（最先到期优先）· 同到期先 Plan 后 Add-on · 失败请求不扣费
          </span>
          {credits?.cached && !credits?.stale && <Badge tone="slate">缓存</Badge>}
          {channels.map((s) =>
            s === 'pat' ? (
              <Badge key={s} tone="green">PAT 通道</Badge>
            ) : s === 'client_token' ? (
              <Badge key={s} tone="blue">客户端凭证</Badge>
            ) : null,
          )}
        </div>
        <div className="rounded-lg border border-slate-200 dark:border-zinc-700">
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-4 py-2 text-left">账号</th>
                <th className="px-4 py-2 text-right">Plan（已用/剩余）</th>
                <th className="px-4 py-2 text-right">Add-on（已用/剩余）</th>
                <th className="px-4 py-2 text-right">合计</th>
                <th className="px-4 py-2 text-left">订阅到期</th>
                <th className="px-4 py-2 text-left">积分包 / 到期</th>
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <tr><td colSpan={6} className="px-3 py-4 text-center text-xs text-slate-400">加载中…</td></tr>
              ) : !credits || credits.accounts.length === 0 ? (
                <tr>
                  <td colSpan={6} className="px-3 py-4 text-center text-xs text-slate-400">
                    {credits?.message || '暂无账号：请先在「账号管理」导入 PAT'}
                  </td>
                </tr>
              ) : (
                credits.accounts.map((a) => (
                  <tr key={a.user_id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                    <td className="px-4 py-3">
                      <div className="font-medium">{a.name}</div>
                      <div className="text-xs text-slate-400">{a.ok ? '' : a.message || '查询失败'}</div>
                    </td>
                    <td className="px-4 py-3 text-right text-xs tabular-nums">
                      {a.plan_used != null || a.plan_credits != null
                        ? `${a.plan_used != null ? fmtCredits(a.plan_used) : '—'} / ${a.plan_credits != null ? fmtCredits(a.plan_credits) : '—'}`
                        : '—'}
                    </td>
                    <td className="px-4 py-3 text-right text-xs tabular-nums">
                      {a.addon_used != null || a.addon_credits != null
                        ? `${a.addon_used != null ? fmtCredits(a.addon_used) : '—'} / ${a.addon_credits != null ? fmtCredits(a.addon_credits) : '—'}`
                        : '—'}
                    </td>
                    <td className="px-4 py-3 text-right text-xs font-medium tabular-nums">
                      {a.total != null ? fmtCredits(a.total) : '—'}
                    </td>
                    <td className="px-4 py-3 text-xs text-slate-500">{a.plan_expires_at || '—'}</td>
                    <td className="px-4 py-3 text-xs text-slate-500">
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
          Plan/Add-on「已用」与订阅到期为 R-7 usage 接口字段；旧缓存缺失时以「—」展示。
        </p>
      </div>
    </div>
  );
}
