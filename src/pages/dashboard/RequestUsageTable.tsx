import { useEffect, useMemo, useState } from 'react';
import { ArrowDown, ArrowUp, ChevronsUpDown } from 'lucide-react';
import { Badge } from '../../components/ui';
import { fmtCredits } from '../../lib/format';
import { REGION_LABELS, type WbRegionFilter } from './adapters';
import type { WbUsageOfficialRequest } from '../../types';

/**
 * 官网请求用量明细表（对齐 Switch-API 积分统计「请求用量」分栏）：
 * 列 = 请求时间 / 账号（含版本徽标）/ 消耗 / 模型 / 客户端 / 请求 ID；
 * 支持表头排序与分页，受页面上方的版本切换与模型筛选联动。
 *
 * 口径诚实：明细为**每账号最近 N 条**（后端截断），合计与趋势走全量聚合——
 * 截断只影响这里能翻到多少条，不影响上方任何数字，故表头显式标注。
 */

type SortKey = 'time' | 'credit' | 'model' | 'client' | 'account';
type SortDir = 'asc' | 'desc';

const PAGE_SIZES = [50, 100] as const;

function sortValue(r: WbUsageOfficialRequest, key: SortKey): string | number {
  switch (key) {
    case 'credit':
      return r.credit;
    case 'model':
      return r.model;
    case 'client':
      return r.client;
    case 'account':
      return r.account_name;
    case 'time':
    default:
      return r.request_time ?? '';
  }
}

function SortableHeader({
  label,
  col,
  sortKey,
  dir,
  onSort,
  align = 'left',
}: {
  label: string;
  col: SortKey;
  sortKey: SortKey;
  dir: SortDir;
  onSort: (k: SortKey) => void;
  align?: 'left' | 'right';
}) {
  const active = sortKey === col;
  const Icon = active ? (dir === 'asc' ? ArrowUp : ArrowDown) : ChevronsUpDown;
  return (
    <th
      scope="col"
      className={`whitespace-nowrap px-2.5 py-2 font-medium ${align === 'right' ? 'text-right' : 'text-left'}`}
      aria-sort={active ? (dir === 'asc' ? 'ascending' : 'descending') : 'none'}
    >
      <button
        type="button"
        onClick={() => onSort(col)}
        className={`inline-flex items-center gap-1 rounded transition-colors hover:text-slate-700 dark:hover:text-zinc-200 ${
          active ? 'font-semibold text-slate-800 dark:text-zinc-100' : ''
        }`}
      >
        <span>{label}</span>
        <Icon size={12} className={active ? '' : 'opacity-45'} />
      </button>
    </th>
  );
}

export default function RequestUsageTable({
  requests,
  region,
  modelFilter,
  detailLimit,
  totalRequests,
}: {
  /** 已按版本/模型筛选过的明细行 */
  requests: WbUsageOfficialRequest[];
  region: WbRegionFilter;
  /** 当前模型筛选（空 = 全部模型） */
  modelFilter: string;
  /** 每账号明细保留上限（后端常量） */
  detailLimit?: number;
  /** 官方口径的全部请求数（不受明细截断影响） */
  totalRequests: number;
}) {
  const [sortKey, setSortKey] = useState<SortKey>('time');
  const [dir, setDir] = useState<SortDir>('desc');
  const [pageSize, setPageSize] = useState<number>(PAGE_SIZES[0]);
  const [page, setPage] = useState(1);

  // 行集合或排序方式一变，旧页码失去意义 → 回第 1 页
  useEffect(() => {
    setPage(1);
  }, [region, modelFilter, sortKey, dir, pageSize, requests.length]);

  const rows = useMemo(() => {
    const sorted = [...requests].sort((a, b) => {
      const va = sortValue(a, sortKey);
      const vb = sortValue(b, sortKey);
      const cmp =
        typeof va === 'number' && typeof vb === 'number'
          ? va - vb
          : String(va).localeCompare(String(vb));
      return dir === 'asc' ? cmp : -cmp;
    });
    return sorted;
  }, [requests, sortKey, dir]);

  const handleSort = (k: SortKey) => {
    if (k === sortKey) {
      setDir((d) => (d === 'asc' ? 'desc' : 'asc'));
      return;
    }
    setSortKey(k);
    // 消耗列默认从高到低（排查「哪几笔最贵」的默认视角），其余列默认升序
    setDir(k === 'credit' ? 'desc' : 'asc');
  };

  const total = rows.length;
  const pages = Math.max(1, Math.ceil(total / pageSize));
  const from = total === 0 ? 0 : (page - 1) * pageSize + 1;
  const to = Math.min(total, page * pageSize);
  const view = rows.slice((page - 1) * pageSize, page * pageSize);

  if (total === 0) {
    return (
      <div className="rounded-lg border border-dashed border-slate-200 px-4 py-8 text-center text-sm text-slate-400 dark:border-zinc-700">
        {modelFilter
          ? `当前筛选（${REGION_LABELS[region]}${modelFilter ? ` · ${modelFilter}` : ''}）下暂无请求用量。`
          : '官方暂无请求用量。'}
      </div>
    );
  }

  return (
    <div>
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2 text-xs text-slate-400">
        <span>
          第 {from}-{to} 条 · 共 {total} 条明细
          {detailLimit ? `（每账号保留最近 ${detailLimit} 条）` : ''}
        </span>
        <span className="font-medium text-slate-600 dark:text-zinc-300">
          官方口径合计 {totalRequests.toLocaleString()} 次请求
        </span>
      </div>
      <div className="-mx-1 overflow-x-auto">
        <table className="w-full min-w-[640px] text-xs">
          <thead className="text-slate-500 dark:text-zinc-400">
            <tr className="border-b border-slate-100 dark:border-zinc-800">
              <SortableHeader label="请求时间" col="time" sortKey={sortKey} dir={dir} onSort={handleSort} />
              <SortableHeader label="账号" col="account" sortKey={sortKey} dir={dir} onSort={handleSort} />
              <SortableHeader label="消耗" col="credit" sortKey={sortKey} dir={dir} onSort={handleSort} align="right" />
              <SortableHeader label="模型" col="model" sortKey={sortKey} dir={dir} onSort={handleSort} />
              <SortableHeader label="客户端" col="client" sortKey={sortKey} dir={dir} onSort={handleSort} />
              <th scope="col" className="whitespace-nowrap px-2.5 py-2 text-left font-medium">
                请求 ID
              </th>
            </tr>
          </thead>
          <tbody>
            {view.map((r) => (
              <tr key={`${r.account_id}-${r.request_id}`} className="border-b border-slate-50 dark:border-zinc-800/60">
                <td className="whitespace-nowrap px-2.5 py-2 text-slate-500 dark:text-zinc-400">
                  {r.request_time}
                </td>
                <td className="max-w-[150px] truncate px-2.5 py-2" title={r.account_name}>
                  <span className="mr-1.5">{r.account_name}</span>
                  <Badge tone={r.region === 'global' ? 'violet' : 'slate'}>
                    {REGION_LABELS[r.region === 'global' ? 'global' : 'cn']}
                  </Badge>
                </td>
                <td className="whitespace-nowrap px-2.5 py-2 text-right font-medium tabular-nums text-amber-600 dark:text-amber-400">
                  {fmtCredits(r.credit)}
                </td>
                <td className="max-w-[190px] truncate px-2.5 py-2" title={r.model}>
                  {r.model}
                </td>
                <td className="max-w-[120px] truncate px-2.5 py-2 text-slate-500 dark:text-zinc-400" title={r.client}>
                  {r.client}
                </td>
                <td
                  className="max-w-[170px] truncate px-2.5 py-2 font-mono text-[10px] text-slate-400"
                  title={r.request_id}
                >
                  {r.request_id}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="mt-3 flex flex-wrap items-center justify-between gap-2 text-xs text-slate-400">
        <div className="flex items-center gap-2">
          <label htmlFor="req-page-size" className="text-slate-500 dark:text-zinc-400">
            每页
          </label>
          <select
            id="req-page-size"
            className="input !w-auto !py-1 text-xs"
            value={pageSize}
            onChange={(e) => setPageSize(Number(e.target.value))}
          >
            {PAGE_SIZES.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </div>
        <div className="flex items-center gap-2">
          <button
            className="btn-outline !py-1 !text-xs"
            onClick={() => setPage((p) => Math.max(1, p - 1))}
            disabled={page <= 1}
          >
            上一页
          </button>
          <span>
            第 {page} / {pages} 页
          </span>
          <button
            className="btn-outline !py-1 !text-xs"
            onClick={() => setPage((p) => Math.min(pages, p + 1))}
            disabled={page >= pages}
          >
            下一页
          </button>
        </div>
      </div>
    </div>
  );
}
