declare module "@novnc/novnc" {
  export default class RFB extends EventTarget {
    constructor(target: HTMLElement, url: string);
    viewOnly: boolean;
    scaleViewport: boolean;
    clipViewport: boolean;
    resizeSession: boolean;
    focus(options?: { preventScroll?: boolean }): void;
    disconnect(): void;
    sendCtrlAltDel(): void;
  }
}
