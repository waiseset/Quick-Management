import { useCallback, useState } from "react";
import type { ReactNode } from "react";

import { chooseExecutables, pickIconFromPath, resolveTarget } from "./api";
import { ItemEditor } from "./components/ItemEditor";
import type { ItemDraft } from "./components/ItemEditor";
import { useApp } from "./store";
import { ALL_CATEGORY_ID, FAVORITES_CATEGORY_ID } from "./types";
import type { IconData, Item, ItemKind } from "./types";
import { useUi } from "./ui";
import { baseName, uid } from "./util";

/** 重名判定用的键：忽略大小写与首尾空格 */
export function sameNameKey(name: string): string {
  return name.trim().toLowerCase();
}

/**
 * 新增条目前的统一守卫。
 * 只要名字撞上已有条目，就先把新旧并排摆出来问一句，确认后才真正写入；
 * 名字没撞、或者根本没有条目时直接执行。
 */
export function useNameConflictGuard() {
  const { data } = useApp();
  const { conflict } = useUi();

  return useCallback(
    (incoming: Item[], proceed: () => void) => {
      const byName = new Map<string, Item[]>();
      for (const item of data.items) {
        const key = sameNameKey(item.name);
        const list = byName.get(key);
        if (list) list.push(item);
        else byName.set(key, [item]);
      }

      const pairs = incoming
        .map((item) => ({ incoming: item, existing: byName.get(sameNameKey(item.name)) ?? [] }))
        .filter((pair) => pair.existing.length > 0);

      if (pairs.length === 0) proceed();
      else conflict({ pairs, onConfirm: proceed });
    },
    [conflict, data.items],
  );
}

export interface AddItemFlow {
  /** 打开「添加条目」编辑器 */
  startAdd: () => void;
  /** 批量添加应用：系统文件对话框里一次选多个 exe */
  bulkAddApps: () => Promise<void>;
  /** 编辑器弹窗；没在添加时是 null */
  editor: ReactNode;
}

/**
 * 「添加条目」的共用流程：主视图与「隐藏的元素」页都走这一份，
 * 唯一差别是条目落进哪一层 —— targetHidden 为 true 时新条目直接进隐藏区。
 */
export function useAddItemFlow(targetHidden = false): AddItemFlow {
  const { data, addItem, showToast, selectedCategoryId, selectedSubCategoryId } = useApp();
  const guard = useNameConflictGuard();
  const [creating, setCreating] = useState<ItemKind | null>(null);

  // 当前选中的分类 / 二级分类就是新条目的归属
  const defaultCategoryIds =
    selectedCategoryId === ALL_CATEGORY_ID || selectedCategoryId === FAVORITES_CATEGORY_ID
      ? []
      : [selectedCategoryId];
  const defaultSubCategoryIds = selectedSubCategoryId ? [selectedSubCategoryId] : [];
  const inFavorites = selectedCategoryId === FAVORITES_CATEGORY_ID;

  /** 批量添加应用：一次选多个文件，逐个提取图标后建条目 */
  const bulkAddApps = async () => {
    const paths = await chooseExecutables();
    if (!paths.length) return;
    showToast(`正在添加 ${paths.length} 个应用…`);
    let order = data.items.reduce((max, entry) => Math.max(max, entry.order + 1), 0);
    const items: Item[] = [];
    for (const path of paths) {
      // 快捷方式先解析到真实目标，避免把 .lnk 本身当成应用
      const target = (await resolveTarget(path)) ?? path;
      let icon: IconData | null = null;
      try {
        icon = await pickIconFromPath(target);
      } catch {
        icon = null;
      }
      items.push({
        id: uid(),
        name: baseName(target),
        description: "",
        kind: "app",
        target,
        categoryIds: defaultCategoryIds,
        subcategoryIds: defaultSubCategoryIds,
        favorite: inFavorites,
        hidden: targetHidden,
        icon,
        order: order++,
        createdAt: Date.now(),
      });
    }
    // 先把这批组好，重名的一次问完再统一写入
    guard(items, () => {
      for (const item of items) addItem(item);
      showToast(`已添加 ${items.length} 个应用`);
    });
  };

  const saveDraft = (draft: ItemDraft) => {
    const order = data.items.reduce((max, entry) => Math.max(max, entry.order + 1), 0);
    const item: Item = {
      id: uid(),
      name: draft.name,
      description: draft.description,
      kind: draft.kind,
      target: draft.target,
      categoryIds: draft.categoryIds,
      subcategoryIds: draft.subcategoryIds,
      // 在收藏夹页面添加的条目直接就是收藏状态
      favorite: draft.favorite || inFavorites,
      hidden: targetHidden,
      icon: draft.icon,
      order,
      createdAt: Date.now(),
    };
    // 撞名字时先问一句；用户取消就留在编辑器里，改个名再存
    guard([item], () => {
      addItem(item);
      setCreating(null);
    });
  };

  const editor = creating ? (
    <ItemEditor
      item={null}
      defaultKind={creating}
      defaultCategoryIds={defaultCategoryIds}
      defaultSubCategoryIds={defaultSubCategoryIds}
      onClose={() => setCreating(null)}
      onSubmit={saveDraft}
    />
  ) : null;

  return { startAdd: () => setCreating("app"), bulkAddApps, editor };
}
