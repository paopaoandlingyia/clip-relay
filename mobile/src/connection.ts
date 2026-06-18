// 连接管理：连接包格式、设备本地持久化、连接测试。
//
// 连接包格式与 web 端保持一致（见 ../../src/lib/mobile-connection.ts）。
// 目前先复制一份类型，等接口稳定后再抽成 web/app 共享的包。

import * as SecureStore from 'expo-secure-store';

export type ConnectionBundle = {
  schemaVersion: 1;
  app: 'clip-relay';
  serverUrl: string;
  apiBase: string;
  accessToken: string;
  generatedAt: string;
};

const STORAGE_KEY = 'clip_relay_connection';

export function parseBundle(raw: string): ConnectionBundle | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!value || typeof value !== 'object') return null;
  const b = value as Partial<ConnectionBundle>;
  const valid =
    b.schemaVersion === 1 &&
    b.app === 'clip-relay' &&
    typeof b.serverUrl === 'string' &&
    typeof b.apiBase === 'string' &&
    typeof b.accessToken === 'string' &&
    typeof b.generatedAt === 'string';
  return valid ? (b as ConnectionBundle) : null;
}

export async function loadConnection(): Promise<ConnectionBundle | null> {
  const raw = await SecureStore.getItemAsync(STORAGE_KEY);
  return raw ? parseBundle(raw) : null;
}

export async function saveConnection(bundle: ConnectionBundle): Promise<void> {
  await SecureStore.setItemAsync(STORAGE_KEY, JSON.stringify(bundle));
}

export async function clearConnection(): Promise<void> {
  await SecureStore.deleteItemAsync(STORAGE_KEY);
}

export type TestResult = { ok: true } | { ok: false; reason: string };

// 先探活（公开接口），再用 token 打一个受保护接口验证凭证。
export async function testConnection(bundle: ConnectionBundle): Promise<TestResult> {
  const base = bundle.apiBase.replace(/\/$/, '');

  try {
    const health = await fetch(`${base}/api/healthz`);
    if (!health.ok) return { ok: false, reason: `服务器无响应（${health.status}）` };
  } catch {
    return { ok: false, reason: '无法连接到服务器，请检查地址与网络' };
  }

  try {
    const res = await fetch(`${base}/api/clipboard`, {
      headers: { Authorization: `Bearer ${bundle.accessToken}` },
    });
    if (res.status === 401 || res.status === 403) {
      return { ok: false, reason: '凭证无效或已过期，请在 web 端重新生成' };
    }
    if (!res.ok) return { ok: false, reason: `接口返回异常（${res.status}）` };
  } catch {
    return { ok: false, reason: '验证凭证时网络出错' };
  }

  return { ok: true };
}
