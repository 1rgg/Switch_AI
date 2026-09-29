import { useCallback, useEffect, useState } from 'react';
import { CalendarClock, RefreshCw, ShieldAlert } from 'lucide-react';
import PageHeader from '../../components/PageHeader';
import { Badge } from '../../components/ui';
import { api } from '../../lib/tauri';
import { useAppStore } from '../../store';
import type { QoderEnvCheck, QoderSettings } from '../../types';

/**
 * qoder-settings 环境配置（F-80 §5.8）：
 * 客户端路径 + 签到/快照调度（app settings）+ 自动签到开关（qoder_settings）
 * + schtasks 注册 + 合规提示（条款风险固定展示，不可跳过）。
 */

const isValidHHMM = (s: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(s.trim());

export default function QoderSettings() {
  const pushToast = useAppStore((s) => s.pushToast);
  const settings = useAppStore((s) => s.settings);
  const saveSettings = useAppStore((s) => s.saveSettings);
  const [qoderSettings, setQoderSettings] = useState<QoderSettings | null>(null);
  const [env, setEnv] = useState<QoderEnvCheck | null>(null);
  const [taskTimes, setTaskTimes] = useState<string[]>([]);
  const [idePath, setIdePath] = useState('');
  const [workPath, setWorkPath] = useState('');
  const [checkinHhmm, setCheckinHhmm] = useState('10:15');
  const [creditsHhmm, setCreditsHhmm] = useState('23:40');
  const [refreshing, setRefreshing] = useState(false);

  const refresh = useCallback(async () => {
    setRefreshing(true);
    try {
      const [qs, ts, e] = await Promise.all([
        api.qoder.settingsGet().catch(() => null),
        api.qoder.checkinTaskStatus().catch(() => [] as string[]),
        api.qoder.envCheck().catch(() => null),
      ]);
      setQoderSettings(qs);
      setTaskTimes(ts);
      setEnv(e);
    } catch (err) {
      pushToast('error', `读取设置失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (settings) {
      setIdePath(settings.qoder_ide_path ?? '');
      setWorkPath(settings.qoderwork_path ?? '');
      setCheckinHhmm(settings.qoder_checkin_hhmm || '10:15');
      setCreditsHhmm(settings.qoder_credits_sync_hhmm || '23:40');
    }
  }, [settings]);

  const saveAppSettings = async (patch: Record<string, unknown>, okMsg: string) => {
    try {
      await saveSettings(patch);
      pushToast('success', okMsg);
    } catch (err) {
      pushToast('error', `保存失败：${String(err)}`);
    }
  };

  const saveQoderSettings = async (next: QoderSettings) => {
    const prev = qoderSettings;
    setQoderSettings(next); // 乐观更新，失败回滚
    try {
      await api.qoder.settingsSet(next);
      pushToast('success', '已保存');
    } catch (err) {
      setQoderSettings(prev);
      pushToast('error', `保存失败：${String(err)}`);
    }
  };

  const registerTask = async () => {
    if (!isValidHHMM(checkinHhmm)) {
      pushToast('error', `签到时刻格式无效：${checkinHhmm}（应为 HH:MM）`);
      return;
    }
    try {
      await api.qoder.checkinTaskRegister([checkinHhmm.trim()]);
      setTaskTimes(await api.qoder.checkinTaskStatus());
      pushToast('success', `Windows 计划任务已注册：每日 ${checkinHhmm}`);
    } catch (err) {
      pushToast('error', `注册失败：${String(err)}`);
    }
  };

  // I20：「存时刻」原直接落库无校验，非法值（如 25:99）会静默入库且调度器无法解析
  const saveCheckinHhmm = async () => {
    if (!isValidHHMM(checkinHhmm)) {
      pushToast('error', `签到时刻格式无效：${checkinHhmm}（应为 HH:MM）`);
      return;
    }
    await saveAppSettings({ qoder_checkin_hhmm: checkinHhmm.trim() }, '签到时刻已保存');
  };

  const saveCreditsHhmm = async () => {
    if (!isValidHHMM(creditsHhmm)) {
      pushToast('error', `快照时刻格式无效：${creditsHhmm}（应为 HH:MM）`);
      return;
    }
    await saveAppSettings({ qoder_credits_sync_hhmm: creditsHhmm.trim() }, '快照时刻已保存');
  };

  const unregisterTask = async () => {
    try {
      await api.qoder.checkinTaskUnregister();
      setTaskTimes(await api.qoder.checkinTaskStatus());
      pushToast('success', 'Windows 计划任务已注销');
    } catch (err) {
      pushToast('error', `注销失败：${String(err)}`);
    }
  };

  return (
    <div className="animate-fade-in">
      <PageHeader
        title="Qoder · 环境配置"
        desc="客户端路径 · 调度 · 自动签到 · 合规提示"
        actions={
          <button className="btn-outline" disabled={refreshing} onClick={() => void refresh()}>
            <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} /> 重新检测
          </button>
        }
      />

      {/* 合规提示（固定展示） */}
      <div className="mb-4 flex items-start gap-2 rounded-lg border border-amber-200 bg-amber-50 p-3 text-xs text-amber-700 dark:border-amber-500/30 dark:bg-amber-500/10 dark:text-amber-300">
        <ShieldAlert size={15} className="mt-0.5 shrink-0" />
        <span>
          合规提示：平台条款对「同一设备 / 手机号 / 支付宝账号」多维去重并限制技术手段自动化参与。
          本功能定位为辅助个人账号的日常领取：单账号默认、多账号需显式开启、
          设备指纹按「每账号稳定绑定」注入（入池生成一次永不轮换，真实捕获值优先透传，
          不做随机轮换）、请求间隔抖动、失败退避。请自行评估并承担条款风险。
        </span>
      </div>

      {/* 双列布局（对齐 BuddySettings：左列环境+路径，右列调度+签到） */}
      <div className="grid items-start gap-4 lg:grid-cols-2">
      {/* 左列：环境检测 + 客户端路径 */}
      <div className="space-y-4">
      <div className="card p-4">
        <div className="mb-3 text-sm font-medium">环境检测</div>
        <div className="grid gap-3 lg:grid-cols-3">
          <div className="rounded-lg border border-slate-100 p-3 text-sm dark:border-zinc-800">
            <div className="flex items-center gap-2 font-medium">
              Qoder CN IDE
              {env?.ide_installed ? <Badge tone="green">已安装</Badge> : <Badge tone="slate">未检测到</Badge>}
              {env?.ide_running && <Badge tone="blue">运行中</Badge>}
            </div>
            <div className="mt-1 truncate text-xs text-slate-400" title={env?.ide_exe ?? undefined}>
              {env?.ide_exe || '%LOCALAPPDATA%\\Programs\\Qoder CN\\Qoder CN.exe'}
            </div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-sm dark:border-zinc-800">
            <div className="flex items-center gap-2 font-medium">
              QoderWork CN
              {env?.qoderwork_installed ? <Badge tone="green">已安装</Badge> : <Badge tone="slate">未检测到</Badge>}
              {env?.qoderwork_running && <Badge tone="blue">运行中</Badge>}
            </div>
            <div className="mt-1 truncate text-xs text-slate-400" title={env?.qoderwork_exe ?? undefined}>
              {env?.qoderwork_exe || '%LOCALAPPDATA%\\Qoder CN\\Qoder CN Launcher\\Qoder CN Launcher.exe'}
            </div>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 text-sm dark:border-zinc-800">
            <div className="flex items-center gap-2 font-medium">
              Qoder CN CLI
              {env?.cli_dir_exists ? <Badge tone="green">已就绪</Badge> : <Badge tone="slate">未安装</Badge>}
            </div>
            <div className="mt-1 truncate text-xs text-slate-400">{env?.cli_dir ? `${env.cli_dir}\\.qoder-cn` : '~/.qoder-cn'}</div>
          </div>
        </div>
      </div>

      {/* 客户端路径 */}
      <div className="card p-4">
        <div className="mb-3 text-sm font-medium">客户端路径（自动识别失败时人工指定）</div>
        <label className="block text-sm">
          <span className="mb-1 block text-xs text-slate-500">
            Qoder CN IDE exe 路径（M3 切换功能使用）
          </span>
          <div className="flex gap-2">
            <input
              className="input w-full font-mono text-xs"
              value={idePath}
              onChange={(e) => setIdePath(e.target.value)}
              placeholder="C:\Users\...\AppData\Local\Programs\Qoder CN\Qoder CN.exe"
            />
            <button
              className="btn-outline shrink-0"
              onClick={() => void saveAppSettings({ qoder_ide_path: idePath.trim() || null }, 'IDE 路径已保存')}
            >
              保存
            </button>
          </div>
        </label>
        <label className="mt-3 block text-sm">
          <span className="mb-1 block text-xs text-slate-500">
            QoderWork CN exe 路径
          </span>
          <div className="flex gap-2">
            <input
              className="input w-full font-mono text-xs"
              value={workPath}
              onChange={(e) => setWorkPath(e.target.value)}
              placeholder="C:\Users\...\AppData\Local\Qoder CN\Qoder CN Launcher\Qoder CN Launcher.exe"
            />
            <button
              className="btn-outline shrink-0"
              onClick={() => void saveAppSettings({ qoderwork_path: workPath.trim() || null }, 'QoderWork 路径已保存')}
            >
              保存
            </button>
          </div>
        </label>
      </div>

      </div>

      {/* 右列：调度 + 签到行为 */}
      <div className="space-y-4">
      <div className="card p-4">
        <div className="mb-3 flex items-center gap-2">
          <CalendarClock size={16} className="text-violet-500" />
          <span className="text-sm font-medium">调度（应用内调度器 + Windows 计划任务双轨）</span>
        </div>
        <div className="grid gap-4 lg:grid-cols-2">
          <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <div className="flex items-center justify-between">
              <span className="text-sm font-medium">每日签到</span>
              {taskTimes.length > 0 ? <Badge tone="green">已注册 {taskTimes.join(' / ')}</Badge> : <Badge tone="slate">计划任务未注册</Badge>}
            </div>
            <div className="mt-2 flex items-center gap-2">
              <input
                className="input w-24 text-center font-mono"
                value={checkinHhmm}
                onChange={(e) => setCheckinHhmm(e.target.value)}
                placeholder="10:15"
              />
              <button className="btn-outline !px-3 !py-1 text-xs" onClick={() => void saveCheckinHhmm()}>
                存时刻
              </button>
              <button className="btn-outline !px-3 !py-1 text-xs" onClick={() => void registerTask()}>
                注册计划任务
              </button>
              {taskTimes.length > 0 && (
                <button className="btn-ghost !px-3 !py-1 text-xs text-rose-500" onClick={() => void unregisterTask()}>
                  注销
                </button>
              )}
            </div>
            <p className="mt-2 text-xs text-slate-400">
              默认 10:15：同时覆盖「0 点签到」与「10:00 登录奖励」双活动；应用开着时调度器自动补跑。
            </p>
          </div>
          <div className="rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <div className="flex items-center justify-between">
              <span className="text-sm font-medium">积分快照</span>
              <label className="flex items-center gap-1.5 text-xs">
                <input
                  type="checkbox"
                  checked={settings?.qoder_credits_sync_enabled ?? true}
                  onChange={(e) => void saveAppSettings({ qoder_credits_sync_enabled: e.target.checked }, e.target.checked ? '快照已开启' : '快照已关闭')}
                />
                启用
              </label>
            </div>
            <div className="mt-2 flex items-center gap-2">
              <input
                className="input w-24 text-center font-mono"
                value={creditsHhmm}
                onChange={(e) => setCreditsHhmm(e.target.value)}
                placeholder="23:40"
              />
              <button className="btn-outline !px-3 !py-1 text-xs" onClick={() => void saveCreditsHhmm()}>
                存时刻
              </button>
            </div>
            <p className="mt-2 text-xs text-slate-400">
              每日拉取全部账号余额写入快照（积分看板趋势数据源）；无账号时空转不计失败。
            </p>
          </div>
        </div>
      </div>

      {/* 自动签到开关（qoder_settings） */}
      <div className="card p-4">
        <div className="mb-3 text-sm font-medium">签到行为</div>
        <div className="grid gap-3 lg:grid-cols-2">
          <label className="flex items-start gap-2 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={qoderSettings?.auto_checkin ?? true}
              onChange={(e) => qoderSettings && void saveQoderSettings({ ...qoderSettings, auto_checkin: e.target.checked })}
            />
            <span className="text-sm">
              自动签到（默认开）
              <span className="block text-xs text-slate-400">启动补签 + 应用内调度器 qoder-checkin 启用判定</span>
            </span>
          </label>
          <label className="flex items-start gap-2 rounded-lg border border-slate-100 p-3 dark:border-zinc-800">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={qoderSettings?.multi_account_enabled ?? false}
              onChange={(e) => qoderSettings && void saveQoderSettings({ ...qoderSettings, multi_account_enabled: e.target.checked })}
            />
            <span className="text-sm">
              多账号签到（默认关，显式开启）
              <span className="block text-xs text-slate-400">
                关闭时每轮仅处理首个账号；开启即代表已知晓并接受平台条款风险。
                每账号绑定一份稳定设备指纹（账号页可查看），签到/积分请求自动注入，
                不同账号以不同设备身份并发，互不影响
              </span>
            </span>
          </label>
        </div>
      </div>
      </div>
      </div>
    </div>
  );
}
