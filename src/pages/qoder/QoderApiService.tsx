import { useCallback, useEffect, useMemo, useState } from 'react';
import { RefreshCw, Save, Coins, ToggleLeft, Activity, Layers } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge, Spinner, StatCard } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import { withMinDelay } from '../../lib/delay';
import type {
  ApiServiceStatus,
  ApiPoolFile,
  PoolStatus,
  UnifiedModel,
  UsageDayView,
  QoderAccountView,
} from '../../types';

/**
 * Qoder · 资源调度（qoder-dispatch-alignment-plan.md §4，P1）
 * 参照 Buddy「资源调度」页同构布局，按 Qoder 实际能力裁剪：
 * - 资源开关仅 qoderEnabled 一个（Qoder v1 无路由级 effort/工具代执行等 wb 同构特性）；
 * - 池成员为 fail-open 全量含凭证账号（后端无独立白名单/分组配置），账号清单只读展示；
 * - 模型目录数据源为统一目录聚合（api.apiServer.unifiedModels 过滤 qoder 源），
 *   手动同步复用每日调度任务入口（qoderCatalogSync）。
 * 网关级功能（服务启停 / 接口配置 / API Keys / 用量统计）在全局 API 管理弹窗，
 * 页内不重复。
 */

export default function QoderApiService() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [status, setStatus] = useState<ApiServiceStatus | null>(null);
  const [pool, setPool] = useState<ApiPoolFile | null>(null);
  const [qoderEnabled, setQoderEnabled] = useState(false);
  const [accounts, setAccounts] = useState<QoderAccountView[]>([]);
  const [models, setModels] = useState<UnifiedModel[]>([]);
  const [syncing, setSyncing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  // 当日活跃账号数据源（今日 qoder 用量桶的账号维度计数）
  const [usage, setUsage] = useState<UsageDayView[]>([]);
  // Qoder 池实时状态（可观测：per-account inflight 在途计数）
  const [qoderPool, setQoderPool] = useState<PoolStatus[]>([]);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [st, pf, accs, cat] = await Promise.all([
        api.apiServer.status().catch(() => null),
        api.apiServer.poolList().catch(() => null),
        api.qoder.accountsList().catch(() => [] as QoderAccountView[]),
        api.apiServer
          .unifiedModels()
          .then((list) => list.filter((m) => m.sources.some((s) => s.pool === 'qoder')))
          .catch(() => [] as UnifiedModel[]),
      ]);
      setStatus(st);
      setPool(pf);
      setAccounts(accs);
      setModels(cat);
      if (pf) {
        setQoderEnabled(pf.qoder_enabled ?? false);
      }
    } catch (err) {
      pushToast('error', `读取资源状态失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 当日 Qoder 用量（query_recent 含今日；仅用于「今日使用」池指标）
  const loadTodayUsage = useCallback(async () => {
    try {
      setUsage(await api.apiServer.qoderUsageStats(1));
    } catch {
      /* 加载失败按无数据处理 */
    }
  }, []);

  useEffect(() => {
    void loadTodayUsage();
  }, [loadTodayUsage]);

  // 服务运行中轮询 Qoder 池实时状态（inflight 徽标；3s 同 Trae/Buddy 页惯例）
  const refreshPoolStatus = useCallback(async () => {
    if (!status?.running) {
      setQoderPool([]);
      return;
    }
    try {
      setQoderPool(await api.apiServer.qoderPoolStatus());
    } catch {
      /* ignore */
    }
  }, [status?.running]);

  useEffect(() => {
    void refreshPoolStatus();
    if (!status?.running) return;
    const id = setInterval(() => void refreshPoolStatus(), 3000);
    return () => clearInterval(id);
  }, [refreshPoolStatus]);

  // 保存资源开关：uids/strategy/groups 原样回传（本页不改 Trae/WB 池配置）；
  // qoderEnabled 随本次保存提交（服务运行中热生效）
  const saveFlags = async () => {
    // 池配置未加载时禁止保存：uids/strategy/groups 原样回传依赖 pool 快照，
    // pool=null 时保存会把 Trae 池 enabled_uids 清空（与 Buddy 页同款防护）
    if (!pool) {
      pushToast('error', '池配置未加载，无法保存（请先刷新重试）');
      return;
    }
    setSaving(true);
    try {
      await withMinDelay(
        api.apiServer.poolSet(pool?.enabled_uids ?? [], pool?.strategy, pool?.group_ids, {
          qoderEnabled,
        }),
        600,
      );
      pushToast('success', 'Qoder 上游开关已保存（服务运行中即时生效）');
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    } finally {
      setSaving(false);
    }
  };

  // 手动同步模型目录：复用每日调度任务入口（按池序逐可用账号拉取 CN 区 model/list）
  const syncCatalog = async () => {
    if (syncing) return;
    setSyncing(true);
    try {
      const n = await withMinDelay(api.apiServer.qoderCatalogSync(), 800);
      pushToast('success', `Qoder 模型目录已更新（${n} 个模型），/v1/models 与路由即时生效`);
      const list = await api.apiServer.unifiedModels();
      setModels(list.filter((m) => m.sources.some((s) => s.pool === 'qoder')));
    } catch (err) {
      pushToast('error', `Qoder 模型目录同步失败：${String(err).slice(0, 120)}`);
    } finally {
      setSyncing(false);
    }
  };

  const credAccounts = useMemo(() => accounts.filter((a) => a.has_credential), [accounts]);

  // 池指标（对齐 Buddy 口径）：健康 = 含凭证且无需重新登录；
  // 今日使用 = 当日被调度使用；池内 = 含凭证账号总数（fail-open 语义即全量）
  const todayKey = new Date().toLocaleDateString('sv-SE');
  const healthyCount = credAccounts.filter((a) => !a.needs_relogin).length;
  const activeToday =
    usage.find((d) => d.date === todayKey)?.accounts.filter((a) => a.requests > 0).length ?? 0;
  const totalQoderCredits = useMemo(
    () => credAccounts.reduce((s, a) => s + (a.credits_balance ?? 0), 0),
    [credAccounts],
  );
  const running = status?.running ?? false;

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 资源调度"
        desc="服务资源池管理 · 上游开关与模型目录 · 资源提供给API网关使用"
        actions={
          <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
          </button>
        }
      />

      {/* 积分体系说明（资源级） */}
      <div className="rounded-xl border border-amber-300/70 bg-amber-50/80 px-3.5 py-2.5 dark:border-amber-700/40 dark:bg-amber-900/10">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-semibold text-amber-800 dark:text-amber-200">积分体系说明</span>
          <span className="rounded bg-amber-200 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-700/50 dark:text-amber-100">本服务消耗 Qoder 通用 credits</span>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-slate-500 dark:text-zinc-400">
          <span>
            Qoder 目录模型按倍率消耗通用 credits（x0.0 免费档 ~ x3.2）；Qoder 源模型的请求由
            Qoder 账号池服务（CN 网关），轮换消耗各账号 credits（模型级排队冷却同现有实现）。
          </span>
          <span className="ml-auto">
            含凭证账号 Qoder credits 总余额：
            <span className="font-bold tabular-nums text-amber-700 dark:text-amber-300">
              {credAccounts.some((a) => a.credits_balance != null)
                ? totalQoderCredits.toLocaleString('zh-CN', { maximumFractionDigits: 2 })
                : '未知'}
            </span>
          </span>
        </div>
      </div>

      {/* 池指标行 */}
      <div className="mt-4 grid grid-cols-3 gap-3">
        <StatCard
          label="健康账号"
          value={healthyCount}
          tone="green"
          hint="含凭证且无需重新登录，可参与调度"
        />
        <StatCard
          label="今日使用"
          value={activeToday}
          tone="blue"
          hint="当日被调度使用过的账号"
        />
        <StatCard
          label="池内账号"
          value={credAccounts.length}
          tone="violet"
          hint="含凭证账号全量自动入池（fail-open）"
        />
      </div>

      {/* 左列：资源开关 + 账号清单｜右列：模型目录（Qoder），同行左右两列各占 1/2 */}
      <div className="mt-4 grid grid-cols-12 items-start gap-4">
        <div className="col-span-6">
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between gap-2">
              <div className="flex items-center gap-2">
                <Activity size={16} className="text-brand-500" />
                <span className="text-sm font-medium">账号池与资源开关</span>
              </div>
              <button className="btn-outline" onClick={() => void saveFlags()} disabled={saving || !pool}>
                {saving ? <Spinner /> : <Save size={15} />} 保存
              </button>
            </div>

            {/* 资源开关（Qoder v1 仅上游总开关；池成员 fail-open 全量入池，无白名单/分组） */}
            <div className="mb-3 space-y-2 rounded-lg bg-slate-50 p-3 dark:bg-zinc-800/50">
              <label className="flex cursor-pointer items-start gap-2.5 rounded-md px-1.5 py-1.5 transition hover:bg-slate-100/60 dark:hover:bg-zinc-800/60">
                <input
                  type="checkbox"
                  className="mt-0.5 h-3.5 w-3.5 rounded border-slate-300 text-brand-600 focus:ring-brand-500"
                  checked={qoderEnabled}
                  onChange={() => setQoderEnabled((v) => !v)}
                />
                <span className="min-w-0">
                  <span className="block text-xs text-slate-700 dark:text-zinc-200">启用 Qoder 上游</span>
                  <span className="block text-[11px] leading-4 text-slate-400 dark:text-zinc-500">
                    Qoder 目录模型路由到 Qoder 账号池（CN 网关），消耗各账号 credits；
                    关闭后仅 Qoder 源模型显式报错，Trae/Buddy 不受影响
                  </span>
                </span>
              </label>
              <div className="px-1.5">
                {qoderEnabled ? (
                  <Badge tone="green">上游已启用</Badge>
                ) : (
                  <Badge tone="amber">上游未启用 — Qoder 源模型将显式报错</Badge>
                )}
              </div>
              <p className="px-1.5 text-[11px] text-slate-400 dark:text-zinc-500">
                池成员为全部含凭证账号自动入池（fail-open），无需勾选；池间调度序为
                Buddy → Trae → Qoder（Qoder 尾部接管），调度策略在全局 API 管理「调度策略中心」配置。
              </p>
              {/* 账号并发上限（只读透传，§4.1）：三池共用热参数，本页不提供编辑 */}
              <div className="flex items-center justify-between px-1.5 pt-1 text-[11px]">
                <span className="text-slate-400 dark:text-zinc-500">账号并发上限</span>
                <span className="tabular-nums text-slate-600 dark:text-zinc-300">
                  {pool?.account_concurrency_limit
                    ? `${pool.account_concurrency_limit} 并发/账号`
                    : '不限（默认）'}
                </span>
              </div>
              <p className="px-1.5 text-[11px] text-slate-400 dark:text-zinc-500">
                三池共用参数（Trae/Buddy/Qoder 同一值，改动影响所有渠道）；如需调整请到
                Buddy「资源调度」页。
              </p>
            </div>

            {/* 账号清单（只读展示：健康状态 / 在途计数 / credits 余额） */}
            {credAccounts.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                暂无含凭证账号：请先在「账号管理」PAT 导入或 OAuth 登录入池（凭证写入本地 token store）。
              </p>
            ) : (
              <div className="space-y-1">
                {credAccounts.map((a) => {
                  // 可观测：实时在途并发（服务未运行/未匹配时为 0）；PoolStatus.uid 与账号 id 同域
                  const inflight = qoderPool.find((p) => p.uid === a.id)?.inflight ?? 0;
                  return (
                    <div
                      key={a.id}
                      className="flex items-center gap-3 rounded-lg border border-slate-100 px-3 py-2 text-sm dark:border-zinc-800"
                    >
                      <div className="min-w-0 flex-1 truncate font-medium">{a.nickname || a.id}</div>
                      <Badge tone="slate">{a.plan || a.credential_source || '—'}</Badge>
                      {a.needs_relogin && <Badge tone="amber">需重新登录</Badge>}
                      {running && inflight > 0 && <Badge tone="amber">在途 {inflight}</Badge>}
                      <span className="shrink-0 text-right tabular-nums text-xs text-slate-500">
                        {a.credits_balance != null ? `${a.credits_balance.toFixed(2)} credits` : '余额未知'}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}

            <p className="mt-3 text-xs text-slate-400 dark:text-zinc-500">
              保存后即时生效；Trae 池的成员/分组与调度策略在 Trae「资源调度」页配置，Buddy 池配置在
              Buddy「资源调度」页，本页不改动。
            </p>
          </div>
        </div>

        {/* 右列：模型目录（Qoder 源，统一目录聚合实时派生） */}
        <div className="col-span-6">
          <div className="card p-4">
            <div className="mb-3 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <Layers size={16} className="text-brand-500" />
                <span className="text-sm font-medium">模型目录（Qoder）</span>
                <span className="text-xs text-slate-400">{models.length} 个模型</span>
              </div>
              <button className="btn-outline" onClick={() => void syncCatalog()} disabled={syncing}>
                <RefreshCw size={14} className={syncing ? 'animate-spin' : ''} />
                {syncing ? '同步中…' : '同步目录'}
              </button>
            </div>
            <p className="mb-3 text-xs text-slate-400">
              从 Qoder CN 网关 model/list 拉取并替换目录缓存（倍率/思考档位/图片模态以服务端为准）；
              应用内置调度器每日自动同步，无账号时静默跳过。
            </p>
            {models.length === 0 ? (
              <p className="py-4 text-center text-xs text-slate-400">
                暂无目录数据：点击「同步目录」拉取（需至少一个含凭证的 Qoder 账号）。
              </p>
            ) : (
              <div className="overflow-x-auto">
                <table className="w-full min-w-[640px] text-sm">
                  <thead className="bg-slate-50 text-xs uppercase text-slate-500 dark:bg-zinc-900">
                    <tr>
                      <th className="px-3 py-2 text-left">模型 ID</th>
                      <th className="px-3 py-2 text-left">展示名</th>
                      <th className="px-3 py-2 text-left">厂商</th>
                      <th className="px-3 py-2 text-center">地区</th>
                      <th className="px-3 py-2 text-right">积分倍率</th>
                      <th className="px-3 py-2 text-left">思考档位</th>
                      <th className="px-3 py-2 text-right">上下文</th>
                      <th className="px-3 py-2 text-center">图片支持</th>
                    </tr>
                  </thead>
                  <tbody>
                    {models.map((m) => (
                      <tr key={m.id} className="row-hover border-t border-slate-200 dark:border-zinc-800">
                        <td className="px-3 py-2 font-mono text-xs">{m.id}</td>
                        <td className="px-3 py-2">{m.display || '—'}</td>
                        <td className="px-3 py-2 text-xs text-slate-500">{m.vendor || '—'}</td>
                        <td className="px-3 py-2 text-center text-xs text-slate-500">
                          {m.region === 'cn' ? 'CN' : m.region === 'global' ? 'Global' : '—'}
                        </td>
                        <td className="px-3 py-2 text-right tabular-nums text-amber-600 dark:text-amber-400">
                          {m.rate != null ? m.rate.toFixed(2) : '—'}
                        </td>
                        <td className="px-3 py-2 text-xs text-slate-500">
                          {m.efforts.length > 0 ? m.efforts.join(' / ') : '—'}
                        </td>
                        <td className="px-3 py-2 text-right tabular-nums text-xs text-slate-500">
                          {m.context_length != null ? `${Math.round(m.context_length / 1000)}k` : '—'}
                        </td>
                        <td className="px-3 py-2 text-center text-xs">
                          {m.supports_image === true ? (
                            <span className="font-semibold text-emerald-600 dark:text-emerald-400">✓</span>
                          ) : (
                            <span className="text-slate-400 dark:text-zinc-500">✗</span>
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            <p className="mt-3 flex items-center gap-1.5 text-xs text-slate-400 dark:text-zinc-500">
              <Coins size={13} className="text-amber-500" />
              倍率为 Qoder 通用 credits 消耗倍率；产品决策下线的模型（Auto / Cantus / Efficient /
              Performance / Sonus / Ultimate）已从目录与路由移除。
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}
