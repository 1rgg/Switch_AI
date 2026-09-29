import { useEffect, useMemo, useState } from 'react';
import { RefreshCw, CheckCircle2, Circle, ChevronRight } from 'lucide-react';
import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
  Cell,
  LabelList,
  Legend,
} from 'recharts';
import PageHeader from '../../components/PageHeader';
import { Spinner, StatCard, Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { useIsDark } from '../../lib/useIsDark';
import type {
  ViewKey,
  QoderAccountView,
  QoderCheckinRecord,
  QoderCreditsResult,
  QoderEnvCheck,
} from '../../types';

/**
 * qoder-overview 概述（F-80 §5.8，对照 BuddyOverview 结构逐块对齐）：
 * 顶部统计卡 + 近 30 天签到结果（堆叠柱状）+ 积分榜 Top 榜 + 配置导航。
 * 差异：无成长中心（Qoder 无对应玩法）；「登录账号/套餐」M1 无客户端在线态数据源，
 * 以客户端环境卡替代（R-2/R-8 闭合后补登录态）。
 */

/** 概述页近 30 天签到趋势数据点（由 QoderCheckinRecord 按日聚合） */
interface TrendPoint {
  date: string;
  ok: number;
  already: number;
  failed: number;
}

/** 配置导航步骤（对齐 Buddy 概述 SetupGuide 形态；optional 步骤不计入完成度） */
interface Step {
  key: string;
  title: string;
  desc: string;
  done: boolean;
  actionLabel: string;
  view: ViewKey;
  optional?: boolean;
}

function aggregateTrends(records: QoderCheckinRecord[]): TrendPoint[] {
  const map = new Map<string, TrendPoint>();
  const seen = new Set<string>(); // 同日同账号去重（手动+定时多轮签到不重复计数）
  for (const r of records) {
    if (!r.date) continue;
    const key = `${r.date}|${r.user_id || r.name}`;
    if (seen.has(key)) continue;
    seen.add(key);
    const p = map.get(r.date) ?? { date: r.date, ok: 0, already: 0, failed: 0 };
    if (r.status === 'success') p.ok += 1;
    else if (r.status === 'already') p.already += 1;
    else p.failed += 1;
    map.set(r.date, p);
  }
  return [...map.values()].sort((a, b) => a.date.localeCompare(b.date));
}

export default function QoderOverview() {
  const pushToast = useAppStore((s) => s.pushToast);
  const setView = useAppStore((s) => s.setView);
  const isDark = useIsDark();
  const [env, setEnv] = useState<QoderEnvCheck | null>(null);
  const [accounts, setAccounts] = useState<QoderAccountView[]>([]);
  const [credits, setCredits] = useState<QoderCreditsResult | null>(null);
  const [records, setRecords] = useState<QoderCheckinRecord[]>([]);
  const [refreshing, setRefreshing] = useState(false);
  const [openingClient, setOpeningClient] = useState(false);
  const [checkinHhmm, setCheckinHhmm] = useState('10:15');

  const refresh = async () => {
    setRefreshing(true);
    try {
      // creditsFetch 失败不拖垮整页（原裸 await 使单路 reject 连带其余三路 setState
      // 全部跳过），同时保留 QoderCredits 页 I18 意图「不内联吞错」：ok/err 包裹后
      // 失败显式 toast 并保留旧数据；其余三路为本地读库/环境探测，降级不阻塞
      const [e, accs, crRes, recs] = await Promise.all([
        api.qoder.envCheck().catch(() => null),
        api.qoder.accountsList().catch(() => [] as QoderAccountView[]),
        api.qoder.creditsFetch().then(
          (cr) => ({ ok: true as const, cr }),
          (err: unknown) => ({ ok: false as const, err }),
        ),
        api.qoder.checkinResults(30).catch(() => [] as QoderCheckinRecord[]),
      ]);
      setEnv(e);
      setAccounts(accs);
      if (crRes.ok) {
        setCredits(crRes.cr);
      } else {
        pushToast('error', `积分查询失败（展示缓存数据）：${String(crRes.err)}`);
      }
      setRecords(recs);
    } catch (err) {
      pushToast('error', `Qoder 概述刷新失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void refresh();
    // 签到调度卡显示全局设置中的签到时刻（原硬编码「每日 10:15」与设置页脱节）
    api.misc
      .settingsGet()
      .then((s) => setCheckinHhmm(s.qoder_checkin_hhmm || '10:15'))
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const total = accounts.length;
  const totalBalance = credits?.accounts.reduce((s, a) => s + (a.total ?? 0), 0) ?? null;
  const okAccounts = credits?.accounts.filter((a) => a.ok).length ?? 0;
  // 今日签到账号数（今日记录去重 user_id）
  const today = new Date().toLocaleDateString('sv-SE');
  const checkedToday = useMemo(
    () => new Set(records.filter((r) => r.date === today && r.status !== 'fail').map((r) => r.user_id)).size,
    [records, today],
  );

  const trends = useMemo(() => aggregateTrends(records), [records]);

  // 客户端环境摘要（Buddy「登录账号」卡的 Qoder 等价物：三端装机/就绪态）
  const envSummary = env
    ? [env.ide_installed ? 'IDE ✓' : 'IDE ✗', env.qoderwork_installed ? 'Work ✓' : 'Work ✗', env.cli_dir_exists ? 'CLI ✓' : 'CLI ✗'].join(' · ')
    : '检测中…';
  const clientReady = !!env && (env.ide_installed || env.qoderwork_installed || env.cli_dir_exists);

  // 告警提醒：Token 24h 内将过期（含已过期）账号数 + 需重新登录账号数
  const nowSec = Math.floor(Date.now() / 1000);
  const tokenSoon = accounts.filter(
    (a) => a.token_expires_at != null && a.token_expires_at <= nowSec + 86400,
  ).length;
  const relogin = accounts.filter((a) => a.needs_relogin).length;
  const alertCount = tokenSoon + relogin;

  // 积分榜 Top（按余额降序，对齐 Buddy 概述）
  const top = useMemo(
    () =>
      [...(credits?.accounts ?? [])]
        .filter((a) => a.total != null && a.total > 0)
        .sort((a, b) => (b.total ?? 0) - (a.total ?? 0))
        .slice(0, 10)
        .map((a) => ({ name: a.name || a.user_id, credits: a.total as number })),
    [credits],
  );

  // 配置导航（步骤完成态实时判定；客户端存储凭证接入受 R-2/R-8 侦察门控，可选步骤）
  const steps: Step[] = [
    {
      key: 'client',
      title: '安装 Qoder 客户端',
      desc: 'IDE / QoderWork / CLI 任一即可（全家桶账号三端共享 Credits）。路径未自动识别时可在「环境配置」人工指定。',
      done: clientReady,
      actionLabel: clientReady ? '打开客户端' : '安装后自动识别',
      view: 'qoder-settings',
    },
    {
      key: 'account',
      title: '导入 PAT 凭证',
      desc: '在 qoder.com.cn/account/integrations 创建 PAT（pt- 前缀，仅关闭页面前可见一次）后到「账号管理」导入。',
      done: accounts.length > 0,
      actionLabel: '去导入',
      view: 'qoder-accounts',
    },
    {
      key: 'checkin',
      title: '完成首次签到',
      desc: '验证签到链路是否跑通（双活动一次覆盖，claim 幂等可放心重试）。',
      done: records.some((r) => r.date === today),
      actionLabel: '去签到',
      view: 'qoder-checkin',
    },
    {
      key: 'credits',
      title: '查询积分余额',
      desc: '录入凭证后查询各账号 Plan / Add-on 积分余额与积分包到期情况。',
      done: (credits?.accounts.length ?? 0) > 0,
      actionLabel: '查看积分',
      view: 'qoder-credits',
    },
    {
      key: 'client-cred',
      title: '客户端本地凭证接入（可选）',
      desc: 'IDE auth.v1.dat / QoderWork 存储自动发现随 M0 R-2/R-8 侦察闭合后开放，届时免 PAT 手工导入。',
      done: false,
      actionLabel: '待侦察闭合',
      view: 'qoder-settings',
      optional: true,
    },
  ];
  // 可选步骤不计入完成度
  const required = steps.filter((s) => !s.optional);
  const completed = required.filter((s) => s.done).length;
  const allDone = completed === required.length;

  // 打开客户端：优先 IDE，其次 QoderWork（后端 spawn；未检测到时降级 warn）
  const openClient = () => {
    setOpeningClient(true);
    const which = env?.ide_installed ? 'ide' : 'work';
    (which === 'ide' ? api.qoder.openIde() : api.qoder.openWork())
      .then(() => pushToast('success', '已启动 Qoder 客户端'))
      .catch((e) => {
        // 对齐 TopBar 分级：「未检测到」是可修复的环境问题（warn），其余才是 error
        const msg = String(e);
        if (msg.includes('未检测到')) pushToast('warn', msg);
        else pushToast('error', `打开客户端失败：${msg}`);
      })
      .finally(() => setOpeningClient(false));
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 概述"
        desc="Qoder CN（IDE / Work / CLI）运行总览 · 客户端环境 / 告警提醒 / 签到趋势与积分榜"
        actions={
          <button onClick={() => void refresh()} className="btn-outline" disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 顶部统计卡（对齐 Buddy 概述：数值一眼掌握） */}
      <div className="grid grid-cols-2 gap-3 md:grid-cols-5">
        <StatCard
          label="账号总数"
          value={total}
          hint={`今日已领 ${checkedToday}`}
          tone="brand"
        />
        <StatCard
          label="可用总积分"
          value={totalBalance != null ? totalBalance.toFixed(2) : '—'}
          hint={credits ? `${okAccounts}/${credits.accounts.length} 个查询成功` : '导入 PAT 后自动查询'}
          tone="amber"
        />
        <StatCard
          label="客户端环境"
          value={clientReady ? '就绪' : '未检测到'}
          hint={envSummary}
          tone="violet"
        />
        <StatCard
          label="签到调度"
          value={`每日 ${checkinHhmm}`}
          hint="单次覆盖 0 点签到 + 10:00 登录奖励"
          tone="violet"
        />
        <StatCard
          label="告警提醒"
          value={alertCount}
          hint={`Token 24h 内过期 ${tokenSoon} · 需重新登录 ${relogin}`}
          tone={alertCount > 0 ? 'red' : 'slate'}
        />
      </div>

      {/* 近 30 天签到结果（对齐 Buddy 概述：堆叠柱状按日汇总） */}
      <div className="mt-5 card p-5">
        <div className="mb-4 flex items-center justify-between">
          <h3 className="font-medium">近 30 天签到结果</h3>
          <span className="text-xs text-slate-400">按日汇总 · 成功 / 已领 / 失败</span>
        </div>
        {trends.length === 0 ? (
          <div className="flex h-40 items-center justify-center text-sm text-slate-400">
            暂无签到记录，完成一次签到后这里会显示趋势。
          </div>
        ) : (
          <div className="h-64">
            <ResponsiveContainer>
              <BarChart data={trends} margin={{ top: 8, right: 16, left: 0, bottom: 4 }} barCategoryGap="24%">
                <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                <XAxis
                  dataKey="date"
                  tickFormatter={(v: string) => v.slice(5)}
                  tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }}
                  axisLine={{ stroke: isDark ? '#3f3f46' : '#e2e8f0' }}
                  tickLine={false}
                />
                <YAxis allowDecimals={false} tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={36} />
                <Tooltip
                  cursor={{ fill: isDark ? 'rgba(255,255,255,0.05)' : 'rgba(0,0,0,0.03)' }}
                  contentStyle={{
                    fontSize: 12,
                    borderRadius: 10,
                    border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
                    background: isDark ? '#18181b' : '#fff',
                    color: isDark ? '#e4e4e7' : '#1e293b',
                    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                    padding: '8px 12px',
                  }}
                />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Bar dataKey="ok" name="成功" stackId="trend" fill="#10b981" maxBarSize={28} />
                <Bar dataKey="already" name="已领" stackId="trend" fill="#0ea5e9" maxBarSize={28} />
                <Bar dataKey="failed" name="失败" stackId="trend" fill="#f43f5e" maxBarSize={28} radius={[4, 4, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* 积分榜 Top 榜（对齐 Buddy 概述） */}
      <div className="mt-5 card p-5">
        <div className="mb-4 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <h3 className="font-medium">积分榜 Top 榜</h3>
            {top.length > 0 && (
              <span className="rounded-full bg-zinc-100 px-2 py-0.5 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                Top {top.length}
              </span>
            )}
          </div>
          <span className="text-xs text-slate-400">按可用积分排序</span>
        </div>
        {top.length === 0 ? (
          <div className="flex h-40 items-center justify-center text-sm text-slate-400">
            暂无积分数据，导入 PAT 并查询积分后展示 Top 榜。
          </div>
        ) : (
          <div className="h-72">
            <ResponsiveContainer>
              <BarChart data={top} margin={{ top: 24, right: 16, left: 0, bottom: 4 }} barCategoryGap="36%">
                <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                <XAxis
                  dataKey="name"
                  tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }}
                  interval={0}
                  angle={-20}
                  textAnchor="end"
                  height={52}
                  axisLine={{ stroke: isDark ? '#3f3f46' : '#e2e8f0' }}
                  tickLine={false}
                />
                <YAxis tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={48} />
                <Tooltip
                  cursor={{ fill: isDark ? 'rgba(255,255,255,0.05)' : 'rgba(0,0,0,0.03)' }}
                  contentStyle={{
                    fontSize: 12,
                    borderRadius: 10,
                    border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
                    background: isDark ? '#18181b' : '#fff',
                    color: isDark ? '#e4e4e7' : '#1e293b',
                    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                    padding: '8px 12px',
                  }}
                  formatter={(v: number) => [v.toFixed(2), '可用积分']}
                />
                <Bar dataKey="credits" radius={[8, 8, 0, 0]} maxBarSize={44}>
                  {top.map((_, i) => {
                    const colors = isDark
                      ? ['#fafafa', '#e4e4e7', '#d4d4d8']
                      : ['#27272a', '#3f3f46', '#52525b'];
                    const fill = i < 3 ? colors[i] : isDark
                      ? `rgba(212,212,216,${Math.max(0.35, 0.6 - (i - 3) * 0.05).toFixed(2)})`
                      : `rgba(82,82,91,${Math.max(0.35, 0.6 - (i - 3) * 0.05).toFixed(2)})`;
                    return <Cell key={i} fill={fill} />;
                  })}
                  <LabelList
                    dataKey="credits"
                    position="top"
                    formatter={(v: number) => (v >= 1000 ? `${(v / 1000).toFixed(1)}k` : v.toFixed(0))}
                    style={{ fontSize: 10, fill: isDark ? '#a1a1aa' : '#94a3b8', fontWeight: 500 }}
                  />
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* 配置导航（对齐 Buddy 概述 SetupGuide 形态） */}
      <div className="mt-5 card overflow-hidden">
        <div className="flex items-center justify-between border-b border-slate-100 px-4 py-3 dark:border-zinc-800">
          <div>
            <h3 className="font-medium">配置导航</h3>
            <p className="text-xs text-slate-500">按步骤完成初始化，已完成的步骤无需重复处理。</p>
          </div>
          <Badge tone={allDone ? 'green' : 'amber'}>
            {completed}/{required.length} 已完成
          </Badge>
        </div>
        <ol className="divide-y divide-slate-100 dark:divide-slate-800">
          {steps.map((step, i) => (
            <li key={step.key} className="flex items-center gap-3 px-4 py-3">
              <div className={step.done ? 'text-emerald-500' : 'text-slate-300 dark:text-zinc-600'}>
                {step.done ? <CheckCircle2 size={20} /> : <Circle size={20} />}
              </div>
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium text-slate-800 dark:text-zinc-100">
                  {i + 1}. {step.title}
                </div>
                <div className="text-xs text-slate-500">{step.desc}</div>
              </div>
              {step.done ? (
                <span className="shrink-0 rounded-full bg-emerald-50 px-2.5 py-1 text-xs font-medium text-emerald-600 dark:bg-emerald-500/15 dark:text-emerald-400">
                  已完成
                </span>
              ) : step.optional ? (
                <span className="shrink-0 rounded-full bg-zinc-100 px-2.5 py-1 text-xs font-medium text-zinc-500 dark:bg-zinc-800 dark:text-zinc-400">
                  可选
                </span>
              ) : (
                <button
                  onClick={() => (step.key === 'client' ? openClient() : setView(step.view))}
                  className="btn-outline shrink-0"
                  disabled={step.key === 'client' && openingClient}
                >
                  {step.key === 'client' && openingClient ? <Spinner /> : null}
                  {step.actionLabel}
                  <ChevronRight size={14} />
                </button>
              )}
            </li>
          ))}
        </ol>
        {/* Qoder 说明（三端审计：签到收益账号级，三端共享 Credits） */}
        <div className="mt-3 border-t border-slate-100 px-4 pt-3 text-xs leading-relaxed text-slate-400 dark:border-zinc-800">
          <b className="font-medium text-slate-500 dark:text-zinc-400">Qoder 说明：</b>
          全家桶账号贯穿 IDE / QoderWork / CLI 三端，Credits 共享消耗（FEFO：最先到期优先）；
          签到收益为账号级 Add-on Credits，任一端领取即全端受益。两条产品线不互通：
          原灵码订单（lingma_ 前缀）的 Credits 不在 QoderWork / CLI 生效。
        </div>
        {allDone && (
          <div className="border-t border-slate-100 bg-emerald-50/60 px-4 py-3 text-sm text-emerald-700 dark:border-zinc-800 dark:bg-emerald-500/10 dark:text-emerald-300">
            全部配置已完成，双活动每日领取交给自动化即可！
          </div>
        )}
      </div>
    </div>
  );
}
