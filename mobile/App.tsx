import { useEffect, useState } from 'react';
import {
  ActivityIndicator,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  View,
} from 'react-native';
import { StatusBar } from 'expo-status-bar';
import {
  clearConnection,
  loadConnection,
  parseBundle,
  saveConnection,
  testConnection,
  type ConnectionBundle,
} from './src/connection';

type Status = 'loading' | 'setup' | 'connected';

export default function App() {
  const [status, setStatus] = useState<Status>('loading');
  const [connection, setConnection] = useState<ConnectionBundle | null>(null);
  const [raw, setRaw] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    loadConnection().then((saved) => {
      if (saved) {
        setConnection(saved);
        setStatus('connected');
      } else {
        setStatus('setup');
      }
    });
  }, []);

  async function handleConnect() {
    setError(null);
    const bundle = parseBundle(raw.trim());
    if (!bundle) {
      setError('连接包格式不正确，请粘贴 web 端「设置」里导出的完整 JSON');
      return;
    }
    setBusy(true);
    const result = await testConnection(bundle);
    setBusy(false);
    if (!result.ok) {
      setError(result.reason);
      return;
    }
    await saveConnection(bundle);
    setConnection(bundle);
    setRaw('');
    setStatus('connected');
  }

  async function handleDisconnect() {
    await clearConnection();
    setConnection(null);
    setStatus('setup');
  }

  if (status === 'loading') {
    return (
      <View style={[styles.container, styles.center]}>
        <ActivityIndicator />
        <StatusBar style="auto" />
      </View>
    );
  }

  if (status === 'connected' && connection) {
    return (
      <View style={styles.container}>
        <ScrollView contentContainerStyle={styles.content}>
          <Text style={styles.title}>已连接</Text>
          <Text style={styles.muted}>{connection.serverUrl}</Text>

          <View style={styles.card}>
            <Text style={styles.placeholderTitle}>条目时间线</Text>
            <Text style={styles.muted}>将在后续里程碑实现</Text>
          </View>

          <Pressable style={styles.secondaryBtn} onPress={handleDisconnect}>
            <Text style={styles.secondaryBtnText}>断开连接</Text>
          </Pressable>
        </ScrollView>
        <StatusBar style="auto" />
      </View>
    );
  }

  return (
    <View style={styles.container}>
      <ScrollView contentContainerStyle={styles.content}>
        <Text style={styles.title}>连接到服务器</Text>
        <Text style={styles.muted}>
          在 web 端「设置 → 设备凭证」导出连接包，把 JSON 内容粘贴到下面。
        </Text>

        <TextInput
          style={styles.input}
          multiline
          autoCapitalize="none"
          autoCorrect={false}
          placeholder='{ "schemaVersion": 1, "app": "clip-relay", ... }'
          value={raw}
          onChangeText={setRaw}
          editable={!busy}
        />

        {error ? <Text style={styles.error}>{error}</Text> : null}

        <Pressable
          style={[styles.primaryBtn, busy && styles.btnDisabled]}
          onPress={handleConnect}
          disabled={busy}
        >
          {busy ? (
            <ActivityIndicator color="#fff" />
          ) : (
            <Text style={styles.primaryBtnText}>粘贴并连接</Text>
          )}
        </Pressable>
      </ScrollView>
      <StatusBar style="auto" />
    </View>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: '#fff' },
  center: { alignItems: 'center', justifyContent: 'center' },
  content: { padding: 24, paddingTop: 72, gap: 16 },
  title: { fontSize: 22, fontWeight: '500', color: '#111' },
  muted: { fontSize: 14, color: '#666', lineHeight: 20 },
  input: {
    minHeight: 140,
    borderWidth: 1,
    borderColor: '#ddd',
    borderRadius: 12,
    padding: 12,
    fontSize: 13,
    fontFamily: 'monospace',
    textAlignVertical: 'top',
    color: '#111',
  },
  error: { fontSize: 13, color: '#c0392b' },
  primaryBtn: {
    backgroundColor: '#185FA5',
    borderRadius: 12,
    paddingVertical: 14,
    alignItems: 'center',
  },
  primaryBtnText: { color: '#fff', fontSize: 15, fontWeight: '500' },
  btnDisabled: { opacity: 0.6 },
  secondaryBtn: {
    borderWidth: 1,
    borderColor: '#ddd',
    borderRadius: 12,
    paddingVertical: 14,
    alignItems: 'center',
  },
  secondaryBtnText: { color: '#333', fontSize: 15 },
  card: {
    borderWidth: 1,
    borderColor: '#eee',
    borderRadius: 12,
    padding: 16,
    gap: 4,
  },
  placeholderTitle: { fontSize: 15, fontWeight: '500', color: '#111' },
});
