import { useMemo } from "react";
import type { MouseEvent } from "react";

import { useDragList } from "../drag";
import { useApp } from "../store";
import type { SubCategory } from "../types";
import { useUi } from "../ui";
import { cx, sortByOrder } from "../util";
import { useContextMenu } from "./ContextMenu";

/**
 * 参考文档第 3 行：二级分类用椭圆胶囊呈现，过多时自动换行。
 *
 * 二级分类**只属于某一个一级分类**：只有选中具体分类时才出现，
 * 「全部」「收藏夹」这类聚合视图直接不渲染（同名子分类不会再被合并到一起）。
 */
export function SubCategoryBar() {
  const {
    categories,
    selectedCategoryId,
    selectedSubCategoryId,
    selectSubCategory,
    addSubCategory,
    renameSubCategory,
    removeSubCategory,
    reorderSubCategories,
    data,
    view,
    sessionSubIds,
  } = useApp();
  const { open } = useContextMenu();
  const { prompt, choose } = useUi();

  const category = categories.find((entry) => entry.id === selectedCategoryId) ?? null;
  // 只有在主视图里才显示「当前所在分类」的选中色
  const isMain = view === "main";

  // 没有任何条目的二级分类不显示；本次运行内刚建的先留着，重开应用时才清理
  const chips = useMemo(() => {
    if (!category) return [];
    const keep = (id: string) =>
      data.items.some((item) => item.subcategoryIds.includes(id)) || sessionSubIds.includes(id);
    return sortByOrder(category.subcategories).filter((sub) => keep(sub.id));
  }, [category, data.items, sessionSubIds]);

  const ids = chips.map((sub) => sub.id);
  const { handlers, activeId, overId, consumeDragClick } = useDragList(ids, (next) => {
    if (category) reorderSubCategories(category.id, next);
  });

  const requestCreate = () => {
    if (!category) return;
    prompt({
      title: `在「${category.name}」下新建子分类`,
      label: "子分类名称",
      placeholder: "例如：编辑器",
      confirmText: "创建",
      onSubmit: (value) => addSubCategory(category.id, value),
    });
  };

  const requestRename = (sub: SubCategory) => {
    if (!category) return;
    prompt({
      title: "重命名子分类",
      label: "子分类名称",
      initialValue: sub.name,
      confirmText: "保存",
      onSubmit: (value) => renameSubCategory(category.id, sub.id, value),
    });
  };

  /** 删除子分类要交代它下面的条目 */
  const requestDelete = (sub: SubCategory) => {
    if (!category) return;
    const count = data.items.filter((item) => item.subcategoryIds.includes(sub.id)).length;
    const apply = (mode: "delete-items" | "keep-items") =>
      removeSubCategory(category.id, sub.id, mode);

    if (count === 0) {
      choose({
        title: "删除子分类",
        message: `确定删除「${sub.name}」吗？`,
        options: [{ key: "keep-items", label: "删除", danger: true }],
        onSelect: () => apply("keep-items"),
      });
      return;
    }
    choose({
      title: "删除子分类",
      message: `「${sub.name}」下还有 ${count} 个条目，要一起删除吗？`,
      options: [
        {
          key: "delete-items",
          label: "一起删除",
          description: `${count} 个条目会同时被删除，无法恢复`,
          danger: true,
        },
        {
          key: "keep-items",
          label: "保留条目",
          description: "条目保留在所属分类中，只是不再属于子分类",
        },
      ],
      onSelect: (key) => apply(key as "delete-items" | "keep-items"),
    });
  };

  const openSubMenu = (event: MouseEvent, sub: SubCategory) => {
    event.preventDefault();
    event.stopPropagation();
    open({
      x: event.clientX,
      y: event.clientY,
      header: (
        <div className="menu-header-block">
          <span className="menu-header-title">{sub.name}</span>
          <span className="menu-header-meta">{category?.name} 下的子分类</span>
        </div>
      ),
      items: [
        { key: "rename", label: "重命名", onSelect: () => requestRename(sub) },
        { key: "delete", label: "删除", danger: true, onSelect: () => requestDelete(sub) },
      ],
    });
  };

  // 「全部」「收藏夹」不是真实的一级分类；隐藏页是独立页面，这两处都不显示胶囊行
  if (!category || view === "vault") return null;

  return (
    <div
      className="subbar"
      onContextMenu={(event) => {
        event.preventDefault();
        open({
          x: event.clientX,
          y: event.clientY,
          items: [{ key: "add", label: "添加", onSelect: requestCreate }],
        });
      }}
    >
      <button
        type="button"
        className={cx("chip", isMain && selectedSubCategoryId === null && "is-active")}
        onClick={() => selectSubCategory(null)}
      >
        全部
      </button>

      {chips.map((sub) => (
        <div
          key={sub.id}
          data-drag-id={sub.id}
          title={category.name}
          className={cx(
            "chip",
            "is-draggable",
            isMain && selectedSubCategoryId === sub.id && "is-active",
            activeId === sub.id && "is-dragging",
            overId === sub.id && "is-drop-target",
          )}
          onClick={() => {
            if (consumeDragClick()) return;
            selectSubCategory(sub.id);
          }}
          onContextMenu={(event) => openSubMenu(event, sub)}
          {...handlers(sub.id)}
        >
          {sub.name}
        </div>
      ))}

      <button type="button" className="chip chip-add" title="新建子分类" onClick={requestCreate}>
        ＋
      </button>
    </div>
  );
}
