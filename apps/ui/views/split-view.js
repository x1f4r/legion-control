// Structural two-pane split view for wide windows (>= 820 px).
// One parent grid owns four shared row tracks (title, tools, label, body) and both panes use
// `grid-template-rows: subgrid`, so baselines and tops align structurally across every tab.

import { html } from '../vendor/preact-htm.js';
import { ScrollContainer } from './scroll-container.js';

export function SplitView({
  leftTitle,
  leftFact = null,
  leftTools = null,
  leftLabel = null,
  leftLabelFact = null,
  leftBody = null,
  onLeftRefresh = null,
  leftBodyRef = null,

  rightTitle = null,
  rightFact = null,
  rightTools = null,
  rightLabel = null,
  rightLabelFact = null,
  rightBody = null,
  rightEmpty = null,
  onRightRefresh = null,
  rightBodyRef = null,
}) {
  const isEmptyRight = Boolean(rightEmpty);

  return html`
    <div class="split">
      <section class="split-pane split-left">
        <div class="split-row split-title">
          <h1 class="pane-title">
            ${typeof leftTitle === 'string'
              ? html`<span class="pane-title-text">${leftTitle}</span>`
              : leftTitle}
          </h1>
          ${leftFact ? html`<span class="split-fact">${leftFact}</span>` : null}
        </div>
        <div class="split-row split-tools">${leftTools || null}</div>
        <div class="split-row split-label">
          ${leftLabel
            ? typeof leftLabel === 'string'
              ? html`<span class="split-label-text">${leftLabel}</span>`
              : leftLabel
            : null}
          ${leftLabelFact
            ? typeof leftLabelFact === 'string'
              ? html`<span class="split-label-fact">${leftLabelFact}</span>`
              : leftLabelFact
            : null}
        </div>
        <${ScrollContainer}
          as="div"
          className="split-row split-body"
          containerRef=${leftBodyRef}
          onRefresh=${onLeftRefresh}
        >
          ${leftBody}
        <//>
      </section>

      <section class="split-pane split-right">
        <div class="split-row split-title" aria-hidden=${isEmptyRight ? 'true' : undefined}>
          ${!isEmptyRight && rightTitle
            ? html`
                <h2 class="pane-title">
                  ${typeof rightTitle === 'string'
                    ? html`<span class="pane-title-text">${rightTitle}</span>`
                    : rightTitle}
                </h2>
              `
            : null}
          ${!isEmptyRight && rightFact
            ? html`<span class="split-fact">${rightFact}</span>`
            : null}
        </div>
        <div class="split-row split-tools" aria-hidden=${isEmptyRight ? 'true' : undefined}>
          ${!isEmptyRight ? rightTools || null : null}
        </div>
        <div class="split-row split-label" aria-hidden=${isEmptyRight ? 'true' : undefined}>
          ${!isEmptyRight && rightLabel
            ? typeof rightLabel === 'string'
              ? html`<span class="split-label-text">${rightLabel}</span>`
              : rightLabel
            : null}
          ${!isEmptyRight && rightLabelFact
            ? typeof rightLabelFact === 'string'
              ? html`<span class="split-label-fact">${rightLabelFact}</span>`
              : rightLabelFact
            : null}
        </div>
        ${isEmptyRight
          ? html`
              <div class="split-row split-body split-body-empty">
                <p class="split-empty">${rightEmpty}</p>
              </div>
            `
          : html`
              <${ScrollContainer}
                as="div"
                className="split-row split-body"
                containerRef=${rightBodyRef}
                onRefresh=${onRightRefresh}
              >
                ${rightBody}
              <//>
            `}
      </section>
    </div>
  `;
}
