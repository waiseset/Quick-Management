import { useState } from "react";

import type { Item } from "../types";
import { ellipsis, iconSrc, itemKindLabel } from "../util";
import { Modal } from "./Modal";

/** 一对冲突：准备新增的条目 + 已经存在的同名条目 */
export interface ConflictPair {
  incoming: Item;
  existing: Item[];
}

/**
 * 「已存在同名条目」弹窗：把新条目与已有的同名条目并排摆出来，
 * 底部的「显示详情」展开描述与完整路径，方便对比完再决定加不加。
 */
export function NameConflictDialog({
  pairs,
  onCancel,
  onConfirm,
}: {
  pairs: ConflictPair[];
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const single = pairs.length === 1;

  return (
    <Modal
      title={single ? "已存在同名条目" : `有 ${pairs.length} 个条目重名`}
      width={520}
      onClose={onCancel}
      footer={
        <>
          <button type="button" className="btn btn-ghost" onClick={() => setExpanded((v) => !v)}>
            {expanded ? "收起详情" : "显示详情"}
          </button>
          <div className="modal-foot-spacer" />
          <button type="button" className="btn" onClick={onCancel}>
            取消
          </button>
          <button type="button" className="btn btn-primary" onClick={onConfirm}>
            仍然添加
          </button>
        </>
      }
    >
      <p className="dialog-message">
        {single
          ? `「${pairs[0].incoming.name}」这个名字已经用过了，确认还要再加一个吗？`
          : `下面 ${pairs.length} 个条目与已有条目同名，确认还要全部添加吗？`}
      </p>
      <p className="dialog-description">只比较名称（忽略大小写与首尾空格），不看类型和路径。</p>

      <div className="conflict-list">
        {pairs.map((pair) => (
          <div key={pair.incoming.id} className="conflict-item">
            <div className="conflict-row">
              <span className="conflict-tag conflict-tag-existing">已有</span>
              <div className="conflict-cards">
                {pair.existing.map((item) => (
                  <ConflictCard key={item.id} item={item} expanded={expanded} />
                ))}
              </div>
            </div>
            <div className="conflict-row">
              <span className="conflict-tag conflict-tag-incoming">新增</span>
              <div className="conflict-cards">
                <ConflictCard item={pair.incoming} expanded={expanded} />
              </div>
            </div>
          </div>
        ))}
      </div>
    </Modal>
  );
}

function ConflictCard({ item, expanded }: { item: Item; expanded: boolean }) {
  const src = iconSrc(item.icon);
  return (
    <div className="conflict-card">
      <div className="conflict-icon">
        {src ? <img src={src} alt="" /> : <span>{itemKindLabel(item.kind)}</span>}
      </div>
      <div className="conflict-info">
        <span className="conflict-name">
          {item.name}
          {item.favorite ? <span className="conflict-star">★</span> : null}
        </span>
        <span className="conflict-meta">{itemKindLabel(item.kind)}</span>
        <span className="conflict-path" title={item.target}>
          {ellipsis(item.target, expanded ? 120 : 52)}
        </span>
        {expanded ? (
          <span className="conflict-desc">描述：{item.description.trim() || "（未填写）"}</span>
        ) : null}
      </div>
    </div>
  );
}
