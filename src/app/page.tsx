'use client';

import { useState, useEffect, useRef } from 'react';
import dynamic from 'next/dynamic';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Card, CardContent, CardHeader } from '@/components/ui/card';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { FileText, Search, Github, Bug, Menu, LogOut } from 'lucide-react';
import { 
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog';
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { useToast } from '@/hooks/use-toast';
import { safeCopyText } from '@/lib/copy';
import { authFetch, verifyPassword, getMobileConnectionBundle, getResolvedApiBase, getStoredAccessToken, getStoredAuthCredential, logout, refreshAccessToken, consumeEmbeddedAccessTokenFromLocation } from '@/lib/auth';
import ThemeSelect from '@/components/ThemeSelect';
import { Sheet, SheetContent, SheetFooter, SheetHeader } from '@/components/ui/sheet';
import { CLIPBOARD_CREATED_EVENT, CLIPBOARD_DELETED_EVENT, CLIPBOARD_REORDERED_EVENT } from '@/lib/socket-events';
import type { ClipboardItem as GridItem } from '@/components/clipboard/ClipboardGrid';

const ClipboardGrid = dynamic(() => import('@/components/clipboard/ClipboardGrid'), { ssr: false });
const ClipboardList = dynamic(() => import('@/components/clipboard/ClipboardList'), { ssr: false });
const AddItemDialog = dynamic(() => import('@/components/clipboard/AddItemDialog'), { ssr: false });
const ItemDetailDialog = dynamic(() => import('@/components/clipboard/ItemDetailDialog'), { ssr: false });
// ShareManagerDialog no longer needed in new flow
const CreateShareDialog = dynamic(() => import('@/components/clipboard/CreateShareDialog'), { ssr: false });
const MobileQuickActions = dynamic(() => import('@/components/clipboard/MobileQuickActions'), { ssr: false });
const MobileOnboardingCard = dynamic(() => import('@/components/clipboard/MobileOnboardingCard'), { ssr: false });

type ClipboardItem = GridItem;

export default function Home() {
  const API_BASE = (process.env.NEXT_PUBLIC_API_BASE || '').replace(/\/$/, '');
  const REPO_URL = 'https://github.com/paopaoandlingyia/clip-relay';
  const ISSUES_URL = 'https://github.com/paopaoandlingyia/clip-relay/issues';
  const [items, setItems] = useState<ClipboardItem[]>([]);
  const [searchTerm, setSearchTerm] = useState('');
  const [isLoading, setIsLoading] = useState(true);
  const [selectedItem, setSelectedItem] = useState<ClipboardItem | null>(null);
  // Track the currently intended detail item and abort in-flight fetches on close
  const selectedIdRef = useRef<string | null>(null);
  const detailAbortRef = useRef<AbortController | null>(null);
  const [authenticated, setAuthenticated] = useState(false);
  // const [shareMgrOpen, setShareMgrOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [checkingAuth, setCheckingAuth] = useState(true);
  const [pendingDeleteId, setPendingDeleteId] = useState<string | null>(null);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [shareItemId, setShareItemId] = useState<string | null>(null);
  const [shareOpen, setShareOpen] = useState(false);
  const [shareInitial, setShareInitial] = useState<{ token: string; url: string } | null>(null);
  const [shareInitialTab, setShareInitialTab] = useState<'status'|'settings'>('status');
  const [qrOpen, setQrOpen] = useState(false);
  const [qrShare, setQrShare] = useState<{ token: string; url: string } | null>(null);
  const [nextCursor, setNextCursor] = useState<{ id: string; createdAt: string; sortWeight?: number } | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [viewMode, setViewMode] = useState<'grid' | 'list'>('grid');
  const { toast } = useToast();

  // 追踪最新的搜索关键字，供 WebSocket 事件回调使用
  const searchTermRef = useRef(searchTerm);
  useEffect(() => {
    searchTermRef.current = searchTerm;
  }, [searchTerm]);

  useEffect(() => {
    if (typeof window === 'undefined') return;
    const url = new URL(window.location.href);
    const imported = url.searchParams.get('shared');
    if (imported !== '1') return;
    toast({
      title: '\u5df2\u5bfc\u5165\u5206\u4eab\u5185\u5bb9',
      description: '\u6765\u81ea\u7cfb\u7edf\u5206\u4eab\u7684\u5185\u5bb9\u5df2\u7ecf\u8fdb\u5165 Clip Relay\u3002',
    });
    url.searchParams.delete('shared');
    window.history.replaceState({}, '', url.pathname + url.search + url.hash);
  }, [toast]);

  // DnD moved into ClipboardGrid

  // 静默鉴权：若已有 Cookie，则自动进入，无需再次输入
  useEffect(() => {
    (async () => {
      try {
        consumeEmbeddedAccessTokenFromLocation();
        const res = await authFetch('/api/health');
        if (res.ok) {
          setAuthenticated(true);
        }
      } catch {}
      setCheckingAuth(false);
    })();
  }, []);

  // 读取并持久化视图模式
  useEffect(() => {
    try {
      const saved = typeof window !== 'undefined' ? localStorage.getItem('clipboard_view_mode') : null;
      if (saved === 'grid' || saved === 'list') {
        setViewMode(saved);
      }
    } catch {}
  }, []);

  useEffect(() => {
    try {
      if (typeof window !== 'undefined') {
        localStorage.setItem('clipboard_view_mode', viewMode);
      }
    } catch {}
  }, [viewMode]);

  // 数据加载完成状态
  const [dataLoaded, setDataLoaded] = useState(false);

  // 获取剪贴板条目数据
  const fetchItems = async (searchTerm = '') => {
    try {
      setIsLoading(true);
      const params = new URLSearchParams();
      params.set('take', '12');
      if (searchTerm) params.set('search', searchTerm);
      const url = `/api/clipboard?${params.toString()}`;
      const response = await authFetch(url);
      if (response.ok) {
        const data = await response.json();
        const list: ClipboardItem[] = Array.isArray(data) ? data : data.items;
        setItems(list);
        setNextCursor(data?.nextCursor ?? null);
      } else {
        toast({
          title: "获取数据失败",
          description: "无法获取剪贴板内容",
          variant: "destructive",
        });
      }
    } catch (error) {
      toast({
        title: "获取数据失败",
        description: "网络错误，请检查连接",
        variant: "destructive",
      });
    } finally {
      setIsLoading(false);
    }
  };

  // 手动触发搜索
  const handleSearch = () => {
    setNextCursor(null);
    fetchItems(searchTerm);
  };

  const loadMore = async () => {
    if (!nextCursor) return;
    try {
      setLoadingMore(true);
      const params = new URLSearchParams();
      params.set('take', '24');
      params.set('cursorCreatedAt', nextCursor.createdAt);
      params.set('cursorId', nextCursor.id);
      if (typeof nextCursor.sortWeight === 'number') params.set('cursorSortWeight', String(nextCursor.sortWeight));
      if (searchTermRef.current) params.set('search', searchTermRef.current);
      const res = await authFetch(`/api/clipboard?${params.toString()}`);
      const data = await res.json();
      if (!res.ok) throw new Error(data?.error || '加载失败');
      const list: ClipboardItem[] = Array.isArray(data) ? data : data.items;
      setItems(prev => [...prev, ...list]);
      setNextCursor(data?.nextCursor ?? null);
    } catch (e: any) {
      toast({ title: '加载失败', description: e?.message || '请稍后重试', variant: 'destructive' });
    } finally {
      setLoadingMore(false);
    }
  };

  // 打开详情：先用现有列表数据即时打开，再并发拉取最新详情
  const openItemById = async (id: string) => {
    try {
      // cancel previous fetch if any
      try { detailAbortRef.current?.abort(); } catch {}
      const ctrl = new AbortController();
      detailAbortRef.current = ctrl;
      const res = await authFetch(`/api/clipboard/${id}`, { signal: ctrl.signal } as any);
      if (res.ok) {
        const data = await res.json();
        // Only update if this item is still intended to be open
        if (selectedIdRef.current === id) {
          setSelectedItem(data);
        }
      } else {
        toast({
          title: '加载失败',
          description: '无法加载条目详情',
          variant: 'destructive',
        });
      }
    } catch (e) {
      // Ignore abort errors; surface real network errors only
      // @ts-expect-error
      const aborted = e?.name === 'AbortError';
      if (!aborted) {
        toast({
          title: '网络错误',
          description: '请检查连接后重试',
          variant: 'destructive',
        });
      }
    }
  };

  // 点击卡片时，优先显示已有数据，提升响应速度
  const handleSelectItem = (id: string) => {
    const local = items.find(i => i.id === id) || null;
    selectedIdRef.current = id;
    if (local) setSelectedItem(local);
    // 后台刷新详情（含 contentType/filePath 等）
    openItemById(id);
  };

  useEffect(() => {
    if (authenticated) {
      fetchItems().finally(() => setDataLoaded(true)); // 初始加载
    }
  }, [authenticated]);

  // SSE 实时同步：在数据加载完成后再建立连接
  useEffect(() => {
    if (!authenticated || !dataLoaded) return;
    let es: EventSource | null = null;
    try {
      // include credentials so auth cookie works on cross-origin if configured
      const useQueryAuth = (process.env.NEXT_PUBLIC_SSE_AUTH_IN_QUERY || '') === '1';
      const pwd = getStoredAuthCredential();
      const url = useQueryAuth && pwd ? `${API_BASE}/api/events?auth=${encodeURIComponent(pwd)}` : `${API_BASE}/api/events`;
      es = new EventSource(url, { withCredentials: true } as any);
      es.addEventListener(CLIPBOARD_CREATED_EVENT, (ev: MessageEvent) => {
        const newItem = JSON.parse((ev as MessageEvent).data) as ClipboardItem;
        if (searchTermRef.current) {
          fetchItems(searchTermRef.current);
        } else {
          setItems(prev => {
            // Insert by sortWeight desc, then createdAt desc, then id desc
            if (prev.find(i => i.id === newItem.id)) return prev;
            const arr = [...prev];
            const keyOf = (x: ClipboardItem) => [x.sortWeight ?? 0, new Date(x.createdAt).getTime(), x.id] as const;
            const newKey = keyOf(newItem);
            let idx = 0;
            while (idx < arr.length) {
              const k = keyOf(arr[idx]);
              if (
                newKey[0] > (k[0] ?? 0) ||
                (newKey[0] === (k[0] ?? 0) && (newKey[1] > k[1] || (newKey[1] === k[1] && newKey[2] > k[2])))
              ) {
                break;
              }
              idx++;
            }
            arr.splice(idx, 0, newItem);
            return arr;
          });
        }
      });
      es.addEventListener(CLIPBOARD_DELETED_EVENT, (ev: MessageEvent) => {
        const { id } = JSON.parse((ev as MessageEvent).data) as { id: string };
        setItems(prev => prev.filter(i => i.id !== id));
      });
      es.addEventListener(CLIPBOARD_REORDERED_EVENT, (ev: MessageEvent) => {
        const data = JSON.parse((ev as MessageEvent).data) as { ids: string[]; weights?: Record<string, number> };
        const order = data?.ids || [];
        const weights = data?.weights || undefined;
        const hasWeights = !!weights && Object.keys(weights).length > 0;
        if (!order.length && !hasWeights) return;
        setItems(prev => {
          let a = [...prev];
          if (hasWeights) {
            // Update local sortWeight and resort to match server ordering logic
            a = a.map(it => (weights && Object.prototype.hasOwnProperty.call(weights, it.id) ? { ...it, sortWeight: weights![it.id] } : it));
            a.sort((x, y) => {
              const kx0 = typeof x.sortWeight === 'number' ? x.sortWeight : 0;
              const ky0 = typeof y.sortWeight === 'number' ? y.sortWeight : 0;
              if (kx0 !== ky0) return ky0 - kx0; // sortWeight desc
              const kx1 = new Date(x.createdAt).getTime();
              const ky1 = new Date(y.createdAt).getTime();
              if (kx1 !== ky1) return ky1 - kx1; // createdAt desc
              if (y.id > x.id) return 1; // id desc
              if (y.id < x.id) return -1;
              return 0;
            });
            return a;
          }
          // Fallback for older servers: use provided ids order only
          const idIndex = new Map(order.map((id, idx) => [id, idx] as const));
          a.sort((x, y) => {
            const ix = idIndex.has(x.id) ? idIndex.get(x.id)! : Number.POSITIVE_INFINITY;
            const iy = idIndex.has(y.id) ? idIndex.get(y.id)! : Number.POSITIVE_INFINITY;
            if (ix !== iy) return ix - iy; // items in payload first, by given order
            return 0;
          });
          return a;
        });
      });
    } catch {}
    return () => { try { es?.close(); } catch {} };
  }, [authenticated, dataLoaded]);

  const copyToClipboard = async (content: string) => {
    const ok = await safeCopyText(content);
    if (ok) {
      toast({ title: '已复制到剪贴板', description: '内容已成功复制到剪贴板' });
    } else {
      toast({ title: '复制失败', description: '浏览器限制或权限不足，请手动复制', variant: 'destructive' });
    }
  };

  // formatting moved to grid/utilities

  const handleDelete = async (id: string) => {
    try {
      const response = await authFetch(`/api/clipboard/${id}`, {
        method: 'DELETE',
      });
      
      if (response.ok) {
        setItems(prev => prev.filter(i => i.id !== id));
        toast({
          title: "已删除",
          description: "条目已成功删除",
        });
      } else {
        toast({
          title: "删除失败",
          description: "无法删除条目",
          variant: "destructive",
        });
      }
    } catch (error) {
      toast({
        title: "删除失败",
        description: "网络错误，请检查连接",
        variant: "destructive",
      });
    }
  };

  if (!authenticated) {
    if (checkingAuth) {
      return (
        <div className="flex items-center justify-center min-h-screen">
          <div className="animate-spin rounded-full h-10 w-10 border-b-2 border-gray-900"></div>
        </div>
      );
    }
    return (
      <AuthDialog
        onSuccess={() => {
          setAuthenticated(true);
          fetchItems();
        }}
      />
    );
  }

  if (isLoading) {
    return (
      <div className="flex items-center justify-center min-h-screen">
        <div className="animate-spin rounded-full h-32 w-32 border-b-2 border-gray-900"></div>
      </div>
    );
  }

  return (
    <div className="min-h-screen bg-background p-4 pb-32 md:p-6 md:pb-6">
      <div className="max-w-7xl mx-auto">
        {/* Header */}
        <div className="relative flex flex-col sm:flex-row gap-4 items-start sm:items-center justify-between mb-6">
          {/* Mobile-only: single settings button at top-right */}
          <div className="sm:hidden absolute top-2 right-2 flex items-center gap-1">
            <Button variant="ghost" size="icon" title="设置" onClick={() => setSettingsOpen(true)}>
              <Menu className="h-5 w-5" />
            </Button>
          </div>
          <div>
            <h1 className="text-3xl font-bold">Clip Relay</h1>
            <p className="text-muted-foreground mt-1">管理您的剪贴板内容</p>
          </div>
          
          <div className="flex flex-col sm:flex-row gap-2 w-full sm:w-auto">
            <div className="flex w-full sm:w-auto gap-2">
              <Input
                placeholder="搜索内容..."
                value={searchTerm}
                onChange={(e) => setSearchTerm(e.target.value)}
                className="w-full sm:w-64"
                disabled={isLoading}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    handleSearch();
                  }
                }}
              />
              <Button onClick={handleSearch} disabled={isLoading}>
                <Search className="h-4 w-4 sm:mr-2" />
                <span className="hidden sm:inline">搜索</span>
              </Button>
            </div>
            <div className="flex gap-2 items-center">
              <div className="hidden sm:block">
                <AddItemDialog
                  onItemAdded={() => fetchItems(searchTerm)}
                  onShareCreated={(share) => { setQrShare({ token: share.token, url: share.url }); setQrOpen(true); }}
                />
              </div>
              {/* 分享管理已整合到详情页，入口暂时隐藏 */}
              {/* Desktop settings */}
              <Button variant="ghost" size="icon" title="设置" className="hidden sm:inline-flex" onClick={() => setSettingsOpen(true)}>
                <Menu className="h-5 w-5" />
              </Button>
            </div>
          </div>
        </div>

        {/* Settings Drawer */}
        <SettingsDrawer
          open={settingsOpen}
          onOpenChange={setSettingsOpen}
          repoUrl={REPO_URL}
          issuesUrl={ISSUES_URL}
          onLogout={() => { setAuthenticated(false); setSelectedItem(null); }}
          viewMode={viewMode}
          onChangeViewMode={setViewMode}
        />

        <MobileOnboardingCard />

        {/* Clipboard Items (grid or list) */}
        {viewMode === 'grid' ? (
          <ClipboardGrid
            items={items}
            onReorder={async (newItems) => {
              // Optimistic update
              setItems(newItems);
              try {
                const ids = newItems.map(i => i.id);
                const res = await authFetch('/api/clipboard/reorder', {
                  method: 'POST',
                  headers: { 'Content-Type': 'application/json' },
                  body: JSON.stringify({ ids })
                });
                if (!res.ok) {
                  throw new Error('reorder failed');
                }
              } catch {
                toast({ title: '排序保存失败', description: '稍后将自动恢复', variant: 'destructive' });
                // Best-effort refresh to reflect server state
                fetchItems(searchTermRef.current || '');
              }
            }}
            onSelectItem={(id: string) => handleSelectItem(id)}
            onCopy={copyToClipboard}
            onRequestDelete={(id: string) => { setPendingDeleteId(id); setDeleteOpen(true); }}
            onRequestShare={(id: string) => { setShareItemId(id); setShareInitialTab('settings'); setShareInitial(null); setShareOpen(true); }}
            onRequestShowQr={async (id: string) => {
              try {
                const res = await authFetch(`/api/clipboard/${id}/share`);
                const data = await res.json();
                if (!res.ok) throw new Error(data?.error || 'failed');
                const origin = typeof window !== 'undefined' ? window.location.origin : '';
                setQrShare({ token: data.token, url: origin + data.url });
                setQrOpen(true);
              } catch {
                toast({ title: '二维码获取失败', variant: 'destructive' });
              }
            }}
          />
        ) : (
          <ClipboardList
            items={items}
            onReorder={async (newItems) => {
              // Optimistic update
              setItems(newItems);
              try {
                const ids = newItems.map(i => i.id);
                const res = await authFetch('/api/clipboard/reorder', {
                  method: 'POST',
                  headers: { 'Content-Type': 'application/json' },
                  body: JSON.stringify({ ids })
                });
                if (!res.ok) {
                  throw new Error('reorder failed');
                }
              } catch {
                toast({ title: '排序保存失败', description: '稍后将自动恢复', variant: 'destructive' });
                // Best-effort refresh to reflect server state
                fetchItems(searchTermRef.current || '');
              }
            }}
            onSelectItem={(id: string) => handleSelectItem(id)}
            onCopy={copyToClipboard}
            onRequestDelete={(id: string) => { setPendingDeleteId(id); setDeleteOpen(true); }}
            onRequestShare={(id: string) => { setShareItemId(id); setShareInitialTab('settings'); setShareInitial(null); setShareOpen(true); }}
            onRequestShowQr={async (id: string) => {
              try {
                const res = await authFetch(`/api/clipboard/${id}/share`);
                const data = await res.json();
                if (!res.ok) throw new Error(data?.error || 'failed');
                const origin = typeof window !== 'undefined' ? window.location.origin : '';
                setQrShare({ token: data.token, url: origin + data.url });
                setQrOpen(true);
              } catch {
                toast({ title: '二维码获取失败', variant: 'destructive' });
              }
            }}
          />
        )}

        {items.length === 0 && (
          <div className="text-center py-12">
            <FileText className="h-12 w-12 text-muted-foreground mx-auto mb-4" />
            <h3 className="text-lg font-medium mb-2">暂无剪贴板内容</h3>
            <p className="text-muted-foreground mb-4">
              {searchTerm ? '没有找到匹配的内容' : '点击上方按钮添加新的剪贴板内容'}
            </p>
            {!searchTerm && (
              <AddItemDialog
                onItemAdded={() => fetchItems(searchTerm)}
                onShareCreated={(share) => { setQrShare({ token: share.token, url: share.url }); setQrOpen(true); }}
              />
            )}
          </div>
        )}

        {items.length > 0 && nextCursor && (
          <div className="flex justify-center mt-6">
            <Button variant="outline" onClick={loadMore} disabled={loadingMore}>
              {loadingMore ? '加载中...' : '加载更多'}
            </Button>
          </div>
        )}
      </div>
      
      {/* 统一删除确认弹窗 */}

      <MobileQuickActions onItemAdded={() => fetchItems(searchTermRef.current || "")} />

      <AlertDialog open={deleteOpen} onOpenChange={setDeleteOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>确认删除</AlertDialogTitle>
            <AlertDialogDescription>删除后将无法恢复。确定要删除该条目吗？</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>取消</AlertDialogCancel>
            <AlertDialogAction onClick={() => { if (pendingDeleteId) handleDelete(pendingDeleteId); setDeleteOpen(false); }}>确认删除</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      {/* 创建分享链接（单例） */}
      <CreateShareDialog
        itemId={shareItemId}
        open={shareOpen}
        initialShare={shareInitial}
        initialTab={shareInitialTab}
        onOpenChange={(o) => {
          setShareOpen(o);
          if (!o) { setShareItemId(null); setShareInitial(null); }
        }}
      />

      {/* 二维码弹窗 */}
      <Dialog open={qrOpen} onOpenChange={setQrOpen}>
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>分享二维码</DialogTitle>
          </DialogHeader>
          {qrShare ? (
            <div className="flex flex-col items-center gap-3">
              <img src={`/api/share/${qrShare.token}/qr?size=320`} alt="二维码" className="border rounded bg-white p-2" width={320} height={320} />
              <div className="text-xs text-muted-foreground break-all text-center">{qrShare.url}</div>
              <div className="flex gap-2">
                <Button variant="outline" onClick={() => window.open(`/api/share/${qrShare.token}/qr?size=1024&download=1`, '_blank')}>下载二维码</Button>
                <Button variant="outline" onClick={async () => { const ok = await safeCopyText(qrShare.url); toast({ title: ok ? '已复制链接' : '请手动复制', variant: ok ? undefined : 'destructive' }); }}>复制链接</Button>
              </div>
            </div>
          ) : (
            <div className="py-6 text-center text-sm text-muted-foreground">加载中...</div>
          )}
        </DialogContent>
      </Dialog>

      {/* 详情对话框 */}
      <ItemDetailDialog
        item={selectedItem}
        open={!!selectedItem}
        onOpenChange={(open) => {
          if (!open) {
            setSelectedItem(null);
            selectedIdRef.current = null;
            try { detailAbortRef.current?.abort(); } catch {}
            detailAbortRef.current = null;
          }
        }}
        onDelete={(id) => { handleDelete(id); setSelectedItem(null); }}
      />

      {/* 分享管理已移除 */}
    </div>
  );
}

// moved into components/clipboard/ClipboardGrid

// 分享管理对话框
// moved into components/clipboard/ShareManagerDialog

// 认证对话框组件
function AuthDialog({ onSuccess }: { onSuccess: () => void }) {
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const { toast } = useToast();

  const handleUnlock = async () => {
    const isValid = await verifyPassword(password);
    if (isValid) {
      toast({
        title: "认证成功",
        description: "欢迎使用 Clip Relay",
      });
      onSuccess();
    } else {
      setError('密码错误，请重试');
    }
  };

  return (
    <div className="flex items-center justify-center min-h-screen bg-background">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <div className="text-2xl font-semibold">需要认证</div>
        </CardHeader>
        <CardContent className="space-y-4">
          <p className="text-muted-foreground">
            请输入访问密码以继续。
          </p>
          <Input
            type="password"
            placeholder="输入密码..."
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            onKeyPress={(e) => e.key === 'Enter' && handleUnlock()}
          />
          {error && <p className="text-sm text-destructive">{error}</p>}
          <Button onClick={handleUnlock} className="w-full">
            解锁
          </Button>
        </CardContent>
      </Card>
    </div>
  );
}

// Settings drawer
function SettingsDrawer({
  open,
  onOpenChange,
  repoUrl,
  issuesUrl,
  onLogout,
  viewMode,
  onChangeViewMode,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  repoUrl: string;
  issuesUrl: string;
  onLogout: () => void;
  viewMode: 'grid' | 'list';
  onChangeViewMode: (m: 'grid' | 'list') => void;
}) {
  const { toast } = useToast();
  const [hasAccessToken, setHasAccessToken] = useState(false);

  useEffect(() => {
    if (open) {
      setHasAccessToken(!!getStoredAccessToken());
    }
  }, [open]);

  const downloadBundle = () => {
    const bundle = getMobileConnectionBundle();
    if (!bundle || typeof window === 'undefined') return false;
    const blob = new Blob([JSON.stringify(bundle, null, 2)], { type: 'application/json' });
    const url = window.URL.createObjectURL(blob);
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = 'clip-relay-connection.json';
    anchor.click();
    window.setTimeout(() => window.URL.revokeObjectURL(url), 0);
    return true;
  };

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="right">
        <SheetHeader>
          <div className="text-lg font-semibold">{"\u8bbe\u7f6e"}</div>
        </SheetHeader>
        <div className="px-4 py-2 space-y-2">
          <Button variant="ghost" className="w-full justify-start" asChild>
            <a href={repoUrl} target="_blank" rel="noopener noreferrer">
              <Github className="h-4 w-4 mr-2" /> {"GitHub \u4ed3\u5e93"}
            </a>
          </Button>
          <Button variant="ghost" className="w-full justify-start" asChild>
            <a href={issuesUrl} target="_blank" rel="noopener noreferrer">
              <Bug className="h-4 w-4 mr-2" /> {"\u63d0\u4ea4\u95ee\u9898"}
            </a>
          </Button>
          <div className="flex items-center justify-between py-2">
            <div className="text-sm">{"\u89c6\u56fe\u6a21\u5f0f"}</div>
            <div>
              <Select value={viewMode} onValueChange={(v) => onChangeViewMode(v as 'grid' | 'list')}>
                <SelectTrigger size="sm">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="grid">{"\u7f51\u683c"}</SelectItem>
                  <SelectItem value="list">{"\u5217\u8868"}</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>
          <div className="flex items-center justify-between py-2">
            <div className="text-sm">{"\u4e3b\u9898"}</div>
            <ThemeSelect />
          </div>
          <div className="rounded-lg border px-3 py-3 space-y-2">
            <div>
              <div className="text-sm font-medium">{"\u8bbe\u5907\u51ed\u8bc1"}</div>
              <p className="mt-1 text-xs text-muted-foreground">
                {`\u4e3a\u540e\u7eed\u79fb\u52a8\u7aef\u8584\u58f3\u9884\u7559\u7684\u957f\u671f\u51ed\u8bc1\u3002\u5f53\u524d\u72b6\u6001\uff1a${hasAccessToken ? "\u5df2\u751f\u6210" : "\u672a\u751f\u6210"}\u3002`}
              </p>
            </div>
            <div className="grid grid-cols-2 gap-2">
              <Button
                variant="outline"
                size="sm"
                className="w-full"
                onClick={async () => {
                  try {
                    const token = await refreshAccessToken();
                    setHasAccessToken(true);
                    const copied = await safeCopyText(token);
                    toast({
                      title: "\u8bbe\u5907\u51ed\u8bc1\u5df2\u5237\u65b0",
                      description: copied
                        ? "\u5df2\u590d\u5236\u5230\u526a\u8d34\u677f\uff0c\u53ef\u76f4\u63a5\u7528\u4e8e\u79fb\u52a8\u7aef\u63a5\u5165\u3002"
                        : "\u5df2\u5237\u65b0\uff0c\u53ef\u7a0d\u540e\u5728\u6b64\u518d\u6b21\u590d\u5236\u3002",
                    });
                  } catch (e: any) {
                    toast({ title: "\u5237\u65b0\u5931\u8d25", description: e.message, variant: "destructive" });
                  }
                }}
              >
                {"\u5237\u65b0\u51ed\u8bc1"}
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="w-full"
                disabled={!hasAccessToken}
                onClick={async () => {
                  const token = getStoredAccessToken();
                  if (!token) return;
                  const copied = await safeCopyText(token);
                  toast({
                    title: copied ? "\u5df2\u590d\u5236\u8bbe\u5907\u51ed\u8bc1" : "\u590d\u5236\u5931\u8d25",
                    description: copied
                      ? "\u53ef\u5728\u79fb\u52a8\u7aef\u914d\u7f6e\u65f6\u76f4\u63a5\u7c98\u8d34\u3002"
                      : "\u8bf7\u624b\u52a8\u590d\u5236\u6d4f\u89c8\u5668\u4e2d\u4fdd\u5b58\u7684\u8bbe\u5907\u51ed\u8bc1\u3002",
                    variant: copied ? undefined : "destructive",
                  });
                }}
              >
                {"\u590d\u5236\u51ed\u8bc1"}
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="w-full"
                onClick={async () => {
                  const copied = await safeCopyText(getResolvedApiBase());
                  toast({
                    title: copied ? "\u5df2\u590d\u5236\u670d\u52a1\u5730\u5740" : "\u590d\u5236\u5931\u8d25",
                    description: copied
                      ? "\u53ef\u5728\u540e\u7eed\u79fb\u52a8\u58f3\u4e2d\u76f4\u63a5\u586b\u5165\u3002"
                      : "\u8bf7\u624b\u52a8\u8bb0\u4e0b\u5f53\u524d\u7ad9\u70b9\u7684 API \u5730\u5740\u3002",
                    variant: copied ? undefined : "destructive",
                  });
                }}
              >
                {"\u590d\u5236\u5730\u5740"}
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="w-full"
                disabled={!hasAccessToken}
                onClick={async () => {
                  const bundle = getMobileConnectionBundle();
                  if (!bundle) return;
                  const copied = await safeCopyText(JSON.stringify(bundle, null, 2));
                  toast({
                    title: copied ? "\u5df2\u590d\u5236\u79fb\u52a8\u7aef\u63a5\u5165\u5305" : "\u590d\u5236\u5931\u8d25",
                    description: copied
                      ? "\u540e\u7eed\u79fb\u52a8\u58f3\u53ef\u76f4\u63a5\u7c98\u8d34\u8fd9\u4e2a JSON \u3002"
                      : "\u8bf7\u624b\u52a8\u590d\u5236\u5f53\u524d\u8bbe\u5907\u7684\u63a5\u5165\u4fe1\u606f\u3002",
                    variant: copied ? undefined : "destructive",
                  });
                }}
              >
                {"\u590d\u5236\u63a5\u5165\u5305"}
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="w-full"
                disabled={!hasAccessToken}
                onClick={() => {
                  const ok = downloadBundle();
                  toast({
                    title: ok ? "\u5df2\u4e0b\u8f7d\u63a5\u5165\u5305" : "\u4e0b\u8f7d\u5931\u8d25",
                    description: ok
                      ? "\u53ef\u7528\u4e8e\u540e\u7eed Android/iPhone \u8584\u58f3\u5bfc\u5165\u3002"
                      : "\u8bf7\u5148\u751f\u6210\u8bbe\u5907\u51ed\u8bc1\u518d\u5bfc\u51fa\u63a5\u5165\u5305\u3002",
                    variant: ok ? undefined : "destructive",
                  });
                }}
              >
                {"\u4e0b\u8f7d\u63a5\u5165\u5305"}
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              {"\u5982\u679c\u7cfb\u7edf\u5206\u4eab\u9762\u677f\u91cc\u8fd8\u6ca1\u6709 Clip Relay\uff0c\u66f4\u65b0\u540e\u8bf7\u5148\u5220\u6389\u65e7\u7684\u4e3b\u5c4f\u5feb\u6377\u65b9\u5f0f\uff0c\u518d\u91cd\u65b0\u5b89\u88c5\u5230\u4e3b\u5c4f\u5e55\u3002"}
            </p>
          </div>
        </div>
        <SheetFooter>
          <Button
            variant="destructive"
            className="w-full justify-center"
            onClick={async () => {
              try {
                await logout();
                toast({ title: "\u5df2\u9000\u51fa\u767b\u5f55" });
                onLogout();
              } catch {}
              onOpenChange(false);
            }}
          >
            <LogOut className="h-4 w-4 mr-2" /> {"\u9000\u51fa\u767b\u5f55"}
          </Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}

// 详情对话框组件
// moved into components/clipboard/ItemDetailDialog

// 添加条目对话框组件已拆分为独立动态组件（见 components/clipboard/AddItemDialog）
