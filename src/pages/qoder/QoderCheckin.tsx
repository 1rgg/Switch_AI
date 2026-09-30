import { useCallback, useEffect, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { CheckCircle2, Gift, PlayCircle, RefreshCw, XCircle } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { QoderAccountView, QoderCheckinRecord } from '../../types';

/**
 * qoder-checkin 每日签到（F-80 §5.8，对照 BuddyCheckin 裁剪复刻）：
 * 签到控制卡（NDJSON 进度）+ 双活动说明卡。
 * 双活动：0 点刷新的 QoderWork 签到（100 Credits/天）+ 10:00 开窗的每日登录奖励
 * （100 Add-on Credits/天）——接口层同源 sash campaigns，一次触发自然全覆盖。
 */

interface QoderAccountLine {
  index: number;
  user_id: string;
  name: string;
  status: 'success' | 'already' | 'fail' | 'skip';
  message?: string;
  reward?: number;
}

type ParsedEvent =
  | { type: 'start'; total: number }
  | { type: 'done'; ok: number; already: number; failed: number }
  | { type: 'exit' }
  | QoderAccountLine
  | { index: 0; status: 'skip'; message: string };

function parseLine(raw: string): ParsedEvent | null {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

function scalarNum(v: unknown): number | null {
  if (typeof v === 'number') return isFinite(v) ? v : null;
  if (typeof v === 'string' && v.trim() !== '') {
    const n = Number(v);
    return isNaN(n) ? null : n;
  }
  return null;
}

function MiniTokenBadge({ a }: { a: QoderAccountView }) {
  if (a.needs_relogin)
    return <span className="text-xs text-rose-500"><XCircle size={11} className="inline" /> 需重新登录</span>;
  const exp = a.token_expires_at;
  if (!exp) return <span className="text-xs text-emerald-500"><CheckCircle2 size={11} className="inline" /> 长期有效</span>;
  const hours = (exp - Math.floor(Date.now() / 1000)) / 3600;
  if (hours <= 0) return <span className="text-xs text-rose-500"><XCircle size={11} className="inline" /> 已过期</span>;
  if (hours <= 24) return <span className="text-xs text-amber-500">{hours.toFixed(1)}h</span>;
  return <span className="text-xs text-emerald-500">{hours.toFixed(0)}h</span>;
}

/** 凭证来源徽标（端类型标签：§3.3 页面内区分凭证来源） */
function SourceBadge({ a }: { a: QoderAccountView }) {
  const label = a.credential_source === 'pat'
    ? 'PAT'
    : a.credential_source === 'ide_store'
    ? 'IDE'
    : a.credential_source === 'qoderwork_store'
    ? 'Work'
    : a.credential_source === 'mitm'
    ? 'MITM'
    : a.credential_source === 'cli'
    ? 'CLI'
    : a.credential_source || '—';
  return <Badge tone={a.credential_source === 'pat' ? 'green' : 'slate'}>{label}</Badge>;
}

export default function QoderCheckin() {
  const pushToast = useAppStore((s) => s.pushToast);
  const [accounts, setAccounts] = useState<QoderAccountView[]>([]);
  const [running, setRunning] = useState(false);
  const [lines, setLines] = useState<QoderAccountLine[]>([]);
  const [doneInfo, setDoneInfo] = useState<{ ok: number; already: number; failed: number } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [checkinMap, setCheckinMap] = useState<Map<string, QoderCheckinRecord[]>>(new Map());
  const [refreshing, setRefreshing] = useState(false);
  const unlistenRef = useRef<(() => void) | null>(null);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [accs, recs] = await Promise.all([
        api.qoder.accountsList().catch(() => [] as QoderAccountView[]),
        api.qoder.checkinResults(1).catch(() => [] as QoderCheckinRecord[]),
      ]);
      setAccounts(accs);
      const today = new Date();
      const todayStr = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
      const m = new Map<string, QoderCheckinRecord[]>();
      for (const r of recs) {
        if (r.date !== todayStr) continue;
        const arr = m.get(r.user_id) ?? [];
        arr.push(r);
        m.set(r.user_id, arr);
      }
      setCheckinMap(m);
    } catch (err) {
      pushToast('error', `读取签到数据失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
    let disposed = false;
    void listen<string>('qoder-checkin-progress', (ev) => {
      const parsed = parseLine(ev.payload);
      if (!parsed || typeof parsed !== 'object') return;
      if ('type' in parsed && parsed.type === 'start') {
        setLines([]);
        setDoneInfo(null);
        setNotice(null);
      } else if ('type' in parsed && parsed.type === 'done') {
        setDoneInfo({ ok: parsed.ok, already: parsed.already, failed: parsed.failed });
        setRunning(false);
        void refresh();
        pushToast(parsed.failed > 0 ? 'warn' : 'success', `Qoder 签到完成：成功 ${parsed.ok}，已签 ${parsed.already}，失败 ${parsed.failed}`);
      } else if ('index' in parsed && parsed.index != null && parsed.index > 0) {
        const line = parsed as QoderAccountLine;
        const reward = scalarNum(line.reward);
        setLines((prev) => {
          const next = prev.slice();
          next[line.index - 1] = { ...line, reward: reward ?? undefined };
          return next;
        });
      } else if ('type' in parsed && parsed.type === 'exit') {
        setRunning(false);
      }
    }).then((u) => {
      if (disposed) u();
      else unlistenRef.current = u;
    });
    return () => {
      disposed = true;
      unlistenRef.current?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startCheckin = async () => {
    if (accounts.length === 0) {
      pushToast('warn', '账号池为空：请先在「账号管理」导入 PAT 或客户端账号');
      return;
    }
    setRunning(true);
    setLines([]);
    setDoneInfo(null);
    try {
      await api.qoder.checkinStart({ skip_checked_in: true });
    } catch (err) {
      setRunning(false);
      pushToast('error', `发起签到失败：${String(err)}`);
    }
  };

  const todayEarnedOf = (uid: string): number | null => {
    const recs = checkinMap.get(uid) ?? [];
    // reward 口径 = 真实入账：fail 记录保留的部分入账计入；
    // already 幂等回放会重复返回当日已领 reward，须排除防重复计数
    const sum = recs.reduce((s, r) => s + (r.status !== 'already' && r.reward != null ? r.reward : 0), 0);
    if (sum > 0) return Math.round(sum * 100) / 100;
    return recs.find((r) => r.status !== 'already' && r.reward != null)?.reward ?? null;
  };
  const checkinStatusOf = (uid: string): 'success' | 'already' | 'fail' | 'skip' | null => {
    const live = lines.find((l) => l.user_id === uid);
    if (live) return live.status;
    const recs = checkinMap.get(uid) ?? [];
    if (recs.some((r) => r.status === 'success' || r.status === 'already')) {
      return recs.some((r) => r.status === 'success') ? 'success' : 'already';
    }
    return recs.find((r) => r.status === 'fail') ? 'fail' : null;
  };

  const earned = Math.round(lines.reduce((s, l) => s + (l.reward ?? 0), 0) * 100) / 100;

  return (
    <div className="animate-fade-in">
      <PageHeader title="Qoder · 每日签到" desc="sash campaigns 幂等领取 · 双活动一次覆盖" />

      {/* 双活动说明卡（§2.2） */}
      {notice && (
        <div className="mb-4 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
          {notice}
        </div>
      )}
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <Gift size={16} className="text-violet-500" />
          <span className="text-sm font-medium">每日双活动（接口层同源，一次触发全覆盖）</span>
        </div>
        <div className="grid gap-3 lg:grid-cols-2">
          <div className="rounded-lg border border-slate-100 p-3 text-sm dark:border-zinc-800">
            <div className="font-medium">QoderWork 每日签到</div>
            <div className="mt-1 text-xs text-slate-400">
              100 Credits/天（独立 30 天有效期包，FEFO 扣减）· 每日 0 点刷新 · 当天未签不补签
            </div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-sm dark:border-zinc-800">
            <div className="font-medium">每日登录奖励</div>
            <div className="mt-1 text-xs text-slate-400">
              100 通用 Add-on Credits/天（仅个人版）· 每日 10:00 开窗至次日 10:00 · 官方暂无结束时间
            </div>
          </div>
        </div>
        <p className="mt-3 text-xs text-slate-400">
          调度默认 10:15 单次覆盖双活动（应用内调度器 + Windows 计划任务双轨，环境配置页可改）；
          claim 天然幂等，重复执行无副作用。
        </p>
      </div>

      {/* 一键签到卡 */}
      <div className="mt-4 card p-4">
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">一键签到</span>
            {running && <Badge tone="blue">执行中</Badge>}
          </div>
          {accounts.length > 0 && !running && <span className="text-xs text-slate-400">已领账号自动跳过（幂等）</span>}
        </div>
        <div className="rounded-lg border border-slate-200 dark:border-zinc-700">
          <table className="w-full text-sm">
            <thead className="bg-slate-50 text-xs text-slate-500 dark:bg-zinc-900">
              <tr>
                <th className="px-3 py-1.5 text-left">账号</th>
                <th className="px-3 py-1.5 text-left">来源</th>
                <th className="px-3 py-1.5 text-left">登录态</th>
                <th className="px-3 py-1.5 text-left">签到状态</th>
                <th className="px-3 py-1.5 text-right">今日获得</th>
                <th className="px-3 py-1.5 text-right">可用总积分</th>
              </tr>
            </thead>
            <tbody>
              {accounts.length === 0 ? (
                <tr>
                  <td colSpan={6} className="px-3 py-4 text-center text-xs text-slate-400">
                    暂无账号：请先在「账号管理」导入 PAT
                  </td>
                </tr>
              ) : (
                accounts.map((a) => {
                  const st = checkinStatusOf(a.id);
                  const earn = lines.find((l) => l.user_id === a.id)?.reward ?? todayEarnedOf(a.id);
                  return (
                    <tr key={a.id} className="border-t border-slate-100 dark:border-zinc-800">
                      <td className="px-3 py-1.5">
                        <div className="font-medium">{a.nickname || a.uid || a.id}</div>
                        <div className="text-xs text-slate-400">{a.phone_masked || a.id}</div>
                      </td>
                      <td className="px-3 py-1.5"><SourceBadge a={a} /></td>
                      <td className="px-3 py-1.5"><MiniTokenBadge a={a} /></td>
                      <td className="px-3 py-1.5">
                        {st == null ? (
                          <Badge tone="slate">未签</Badge>
                        ) : st === 'success' ? (
                          <Badge tone="green">已领</Badge>
                        ) : st === 'already' ? (
                          <Badge tone="blue">已领（此前已领）</Badge>
                        ) : st === 'skip' ? (
                          <Badge tone="amber">跳过</Badge>
                        ) : (
                          <Badge tone="red">失败</Badge>
                        )}
                      </td>
                      <td className="px-3 py-1.5 text-right tabular-nums text-xs">
                        {earn != null ? (
                          <span className="text-emerald-600 dark:text-emerald-400">+{earn}</span>
                        ) : (
                          <span className="text-slate-300 dark:text-zinc-600">—</span>
                        )}
                      </td>
                      <td className="px-3 py-1.5 text-right tabular-nums text-xs">
                        {a.credits_balance != null ? a.credits_balance.toLocaleString() : '-'}
                      </td>
                    </tr>
                  );
                })
              )}
            </tbody>
          </table>
        </div>
        <div className="mt-3 flex items-center justify-between">
          <span className="text-xs text-slate-400">
            {running ? '签到进行中，逐账号结果见下方实时进度…' : '签到结果将展示在下方实时进度卡'}
          </span>
          <div className="flex items-center gap-2">
            <button className="btn-outline" onClick={() => void refresh()} disabled={refreshing}>
              <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 刷新
            </button>
            <button className="btn-outline" onClick={() => void startCheckin()} disabled={running}>
              <PlayCircle size={15} /> {running ? '签到中…' : '开始签到'}
            </button>
          </div>
        </div>
      </div>

      {/* 实时进度卡 */}
      {(running || lines.length > 0) && (
        <div className="mt-4 card p-4">
          <div className="mb-3 flex items-center justify-between">
            <h3 className="font-medium">实时进度</h3>
            {running ? (
              <Badge tone="blue">运行中</Badge>
            ) : doneInfo ? (
              <Badge tone={doneInfo.failed > 0 ? 'amber' : 'green'}>
                完成：成功 {doneInfo.ok} · 已签 {doneInfo.already} · 失败 {doneInfo.failed}
                {earned > 0 && ` · 获得 ${earned} Credits`}
              </Badge>
            ) : null}
          </div>
          <div className="space-y-1">
            {/* 过滤稀疏数组空洞：乱序事件按 index 跳写产生 hole，直接 map 会在 hole 上取 status 崩溃 */}
            {lines
              .filter((l) => l && l.user_id)
              .map((l) => {
                const tone =
                  l.status === 'success'
                    ? 'text-emerald-600 dark:text-emerald-300'
                    : l.status === 'already'
                    ? 'text-sky-600 dark:text-sky-300'
                    : 'text-rose-600 dark:text-rose-300';
                const Icon = l.status === 'fail' ? XCircle : CheckCircle2;
                return (
                  <div key={l.index} className="flex items-center gap-2 rounded border border-slate-200 px-3 py-2 text-sm dark:border-zinc-700">
                  <Icon size={14} className={tone} />
                  <span className="w-8 text-right text-xs text-slate-400">{l.index}</span>
                  <span className="flex-1 truncate">{l.name || l.user_id}</span>
                  <span className={`max-w-[50%] truncate text-xs ${tone}`} title={l.message}>
                    {l.message || l.status}
                  </span>
                  {l.reward != null && (
                    <span className="shrink-0 rounded bg-emerald-50 px-1.5 py-0.5 text-xs font-medium text-emerald-600 dark:bg-emerald-500/10 dark:text-emerald-300">
                      +{l.reward}
                    </span>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}
