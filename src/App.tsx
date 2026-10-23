import { useCallback, useEffect } from "react";
import { listen } from "@tauri-apps/api/event";

import { useNameConflictGuard } from "./addItem";
import { fetchFavicon, isTauri, pickIconFromPath } from "./api";
import { AboutView } from "./components/AboutView";
import { ContextMenuProvider } from "./components/ContextMenu";
import { DataView } from "./components/DataView";
import { ItemGrid } from "./components/ItemGrid";
import { ReminderView } from "./components/ReminderView";
import { Sidebar } from "./components/Sidebar";
import { SubCategoryBar } from "./components/SubCategoryBar";
import { TitleBar } from "./components/TitleBar";
import { VaultView } from "./components/VaultView";
import { AppProvider, useApp } from "./store";
import { ALL_CATEGORY_ID, FAVORITES_CATEGORY_ID } from "./types";
import type { DroppedEntry, IconData, Item } from "./types";
import { UiProvider } from "./ui";
import { uid } from "./util";

/**
 * 整体骨架与参考文档一致：
 * 顶部自定义标题栏（左上角应用名 -> 菜单，右上角窗口按钮），
 * 左侧 A 列分类栏，右侧依次是二级分类胶囊行与内容区。
 */
function Shell() {
  const {
    ready,
    view,
    setView,
    toast,
    data,
    addItem,
    selectedCategoryId,
    selectedSubCategoryId,
    vaultUnlocked,
    showToast,
  } = useApp();
  const guard = useNameConflictGuard();

  /** 从资源管理器拖进来的文件：Rust 已解析好快捷方式，这里取图标并建条目 */
  const handleDropped = useCallback(
    async (entries: DroppedEntry[]) => {
      if (!entries.length) return;
      // 隐藏页与收藏夹都支持「就地添加」，其余视图拖进来先回主视图
      const intoVault = view === "vault" && vaultUnlocked;
      const intoFavorites = view === "main" && selectedCategoryId === FAVORITES_CATEGORY_ID;
      if (!intoVault && !intoFavorites) setView("main");
      const inCategory =
        selectedCategoryId !== ALL_CATEGORY_ID && selectedCategoryId !== FAVORITES_CATEGORY_ID;
      let order = data.items.reduce((max, item) => Math.max(max, item.order + 1), 0);
      const items: Item[] = [];
      for (const entry of entries) {
        let icon: IconData | null = null;
        try {
          icon =
            entry.kind === "url"
              ? await fetchFavicon(entry.target)
              : await pickIconFromPath(entry.target);
        } catch {
          icon = null;
        }
        items.push({
          id: uid(),
          name: entry.name,
          description: "",
          kind: entry.kind === "url" ? "url" : entry.kind === "file" ? "file" : "app",
          target: entry.target,
          categoryIds: inCategory ? [selectedCategoryId] : [],
          // 二级分类只属于一个一级分类，选中的那个就是目标
          subcategoryIds: inCategory && selectedSubCategoryId ? [selectedSubCategoryId] : [],
          // 在收藏夹页面拖入的条目直接是收藏状态
          favorite: intoFavorites,
          hidden: intoVault,
          icon,
          order: order++,
          createdAt: Date.now(),
        });
      }
      // 一次拖进好几个时，重名的一次问完再统一写入
      guard(items, () => {
        for (const item of items) addItem(item);
        showToast(
          intoVault
            ? `已把 ${items.length} 个条目加进「隐藏的元素」`
            : intoFavorites
              ? `已把 ${items.length} 个条目加进收藏夹`
              : `已从拖入的文件添加 ${items.length} 个条目`,
        );
      });
    },
    [
      addItem,
      data.items,
      guard,
      selectedCategoryId,
      selectedSubCategoryId,
      setView,
      showToast,
      view,
      vaultUnlocked,
    ],
  );

  useEffect(() => {
    if (!isTauri()) return;
    let alive = true;
    let unlisten: (() => void) | undefined;
    void listen<DroppedEntry[]>("files-dropped", (event) => {
      void handleDropped(event.payload);
    }).then((fn) => {
      if (alive) unlisten = fn;
      else fn();
    });
    return () => {
      alive = false;
      unlisten?.();
    };
  }, [handleDropped]);

  // Esc 返回主视图；有弹窗开着时交给弹窗自己处理
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (document.querySelector(".modal-backdrop, .context-menu")) return;
      if (view !== "main") setView("main");
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [view, setView]);

  return (
    <div className="app">
      <TitleBar />
      <div className="app-body">
        <Sidebar />
        <main className="content">
          {/* 二级分类胶囊属于分类筛选，隐藏页是独立页面，这里只在主视图显示 */}
          {view === "main" ? <SubCategoryBar /> : null}
          {view === "main" ? <ItemGrid /> : null}
          {view === "reminder" ? <ReminderView /> : null}
          {view === "vault" ? <VaultView /> : null}
          {view === "data" ? <DataView /> : null}
          {view === "about" ? <AboutView /> : null}
        </main>
      </div>
      {toast ? <div className="toast">{toast}</div> : null}
      {!ready ? <div className="boot">正在载入数据…</div> : null}
    </div>
  );
}

export default function App() {
  return (
    <AppProvider>
      <UiProvider>
        <ContextMenuProvider>
          <Shell />
        </ContextMenuProvider>
      </UiProvider>
    </AppProvider>
  );
}
