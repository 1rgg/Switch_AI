// 软件宣传图 base64 内嵌（由 scripts/gen_asset_base64.mjs 从 promo_banner.jpg 生成）：
// dev server 关闭后弹窗图片仍可显示，不依赖运行中的静态服务器
import { promo_banner_base64 as promoBanner } from '../assets/promo-banner.base64';

/** 应用品牌与关于信息（集中管理，改名只改这里；版本号不再硬编码——运行时经 getVersion() 读 Cargo.toml 单一来源） */
export const APP_NAME = 'Switch AI';
export const APP_TAGLINE = '多账号签到与管理 · 一站式工作台';
export const APP_OVERVIEW =
  'Windows 桌面端 AI 编程工具多账号签到与管理一站式工作台（Tauri 2 + React 18 + Rust）。' +
  '支持 Trae Work / Trae（Trae CN IDE）、WorkBuddy（国内版 + 国际版 workbuddy.ai）、Qoder 等多平台：' +
  '多账号签到、登录态独立切换、设备隔离、积分看板与 OpenAI / Anthropic 兼容 API 网关，数据全部本地存储。';
export const APP_AUTHOR = '朱天伟';
export const APP_COPYRIGHT = `Copyright © 2026 ${APP_AUTHOR} · MIT License`;
export const APP_CREDIT =
  '本项目基于 AI Work 助手（TraeWorkAssistant）二次开发，新增 WorkBuddy 国际版（workbuddy.ai）支持。';
export const APP_DISCLAIMER =
  '本工具与 Trae / WorkBuddy / CodeBuddy / Qoder / 豆包等官方均无关联，仅供学习研究，请仅管理本人合法持有的账号，风险自担。';

export const LINK_GITHUB = 'https://github.com/1rgg';
export const LINK_BLOG = 'https://github.com/1rgg/Switch_AI';
export const LINK_REPO = 'https://github.com/1rgg/Switch_AI';

export { promoBanner };
