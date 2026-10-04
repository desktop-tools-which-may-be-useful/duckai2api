
(function () {
  "use strict";
  var userAgent__ = __goUserAgent;

  // ===== TextEncoder =====
  function TextEncoder__() {}
  TextEncoder__.prototype.encode = function (value) {
    var text = String(value);
    var encoded = encodeURIComponent(text);
    var bytes = [];
    for (var i = 0; i < encoded.length; i++) {
      if (encoded[i] === "%") {
        bytes.push(parseInt(encoded.slice(i + 1, i + 3), 16));
        i += 2;
      } else {
        bytes.push(encoded.charCodeAt(i));
      }
    }
    return new Uint8Array(bytes);
  };

  // ===== Navigator =====
  var navigator__ = {
    userAgent: userAgent__,
    platform: "Win32",
    language: "en-US",
    languages: ["en-US", "en"],
    cookieEnabled: true,
    onLine: true,
    hardwareConcurrency: 4,
    maxTouchPoints: 0,
    vendor: "Google Inc.",
    vendorSub: "",
    productSub: "20030107",
    appCodeName: "Mozilla",
    appName: "Netscape",
    appVersion: userAgent__,
    product: "Gecko",
    doNotTrack: null,
    mimeTypes: { length: 0, item: function () { return null; }, namedItem: function () { return null; } },
    plugins: { length: 0, item: function () { return null; }, namedItem: function () { return null; } },
    webdriver: false,
    deviceMemory: 8,
    javaEnabled: function () { return false; },
    getBattery: function () { return Promise.resolve({ level: 1, charging: true }); },
  };

  // ===== DOM constructors =====
  function NodeList__(items) {
    var vals = items || [];
    for (var i = 0; i < vals.length; i++) this[i] = vals[i];
    this.length = vals.length;
  }
  NodeList__.prototype.item = function (i) { return this[i] || null; };
  NodeList__.prototype.forEach = function (fn) {
    for (var i = 0; i < this.length; i++) fn(this[i], i, this);
  };

  function Element__(tagName) {
    var name = String(tagName || "").toUpperCase();
    this.tagName = name;
    this.nodeType = 1;
    this.nodeName = name;
    this.children = [];
    this.parentNode = null;
    this.ownerDocument = null;
    this.attributes = {};
    this.innerHTML = "";
    this.textContent = "";
    this.srcdoc = "";
    this.src = "";
    this.style = {
      cssText: "",
      display: "inline-block",
      getPropertyValue: function (n) {
        return String(n).toLowerCase() === "display" ? this.display || "inline-block" : "";
      },
    };
    this.offsetWidth = 1;
    this.offsetHeight = 1;
    this.scrollHeight = 1;
    this.clientWidth = 1;
    this.clientHeight = 1;
  }
  Element__.prototype.constructor = Element__;
  Element__.prototype.appendChild = function (child) {
    child.parentNode = this;
    child.ownerDocument = this.ownerDocument;
    this.children.push(child);
    return child;
  };
  Element__.prototype.removeChild = function (child) {
    var idx = this.children.indexOf(child);
    if (idx >= 0) { this.children.splice(idx, 1); child.parentNode = null; }
    return child;
  };
  // 内部函数: 匹配属性选择器 meta[http-equiv="..."]
  function matchesAttributeSelector__(el, selector) {
    var re = /^([a-z0-9_-]+)\[([a-z0-9_-]+)=(["']?)([^"'\]]+)\3\]$/i;
    var m = selector.match(re);
    if (!m) return false;
    var tag = m[1].toLowerCase(), attr = m[2], val = m[4];
    if (tag !== "*" && el.tagName && el.tagName.toLowerCase() !== tag) return false;
    return el.getAttribute(attr) === val;
  }

  // 内部函数: 递归收集匹配元素
  function collectMatching__(el, matchFn, results) {
    if (matchFn(el)) results.push(el);
    if (el.children && el.children.length > 0) {
      for (var i = 0; i < el.children.length; i++) {
        collectMatching__(el.children[i], matchFn, results);
      }
    }
  }

  Element__.prototype.querySelectorAll = function (selector) {
    selector = String(selector || "").toLowerCase();
    // 特殊: #jsa
    if (selector === "#jsa" && this.ownerDocument && this.ownerDocument.__jsa__) {
      return new NodeList__([this.ownerDocument.__jsa__]);
    }
    // 特殊: meta[http-equiv="Content-Security-Policy"]
    if (selector.indexOf("meta[") === 0) {
      var results = [];
      if (this.children && this.children.length > 0) {
        for (var i = 0; i < this.children.length; i++) {
          collectMatching__(this.children[i], function(el) {
            return matchesAttributeSelector__(el, selector);
          }, results);
        }
      }
      return new NodeList__(results);
    }
    return new NodeList__([]);
  };
  Element__.prototype.querySelector = function (selector) {
    var list = this.querySelectorAll(selector);
    return list.length > 0 ? list[0] : null;
  };
  Element__.prototype.getAttribute = function (name) {
    return this.attributes[String(name)] || null;
  };
  Element__.prototype.setAttribute = function (name, value) {
    this.attributes[String(name)] = String(value);
  };
  Element__.prototype.getBoundingClientRect = function () {
    return { width: 1, height: 1, top: 0, right: 1, bottom: 1, left: 0 };
  };
  Element__.prototype.addEventListener = function () {};
  Element__.prototype.removeEventListener = function () {};
  Element__.prototype.focus = function () {};
  Element__.prototype.blur = function () {};
  Element__.prototype.cloneNode = function () { return Object.create(this); };

  function HTMLElement__() { Element__.apply(this, arguments); }
  HTMLElement__.prototype = Object.create(Element__.prototype);
  HTMLElement__.prototype.constructor = HTMLElement__;

  function HTMLDivElement__() { HTMLElement__.apply(this, arguments); }
  HTMLDivElement__.prototype = Object.create(HTMLElement__.prototype);
  HTMLDivElement__.prototype.constructor = HTMLDivElement__;

  function HTMLIFrameElement__() { HTMLElement__.apply(this, arguments); }
  HTMLIFrameElement__.prototype = Object.create(HTMLElement__.prototype);
  HTMLIFrameElement__.prototype.constructor = HTMLIFrameElement__;

  function HTMLScriptElement__() { HTMLElement__.apply(this, arguments); }
  HTMLScriptElement__.prototype = Object.create(HTMLElement__.prototype);
  HTMLScriptElement__.prototype.constructor = HTMLScriptElement__;

  function createElement__(tagName) {
    tagName = String(tagName || "").toLowerCase();
    var el;
    if (tagName === "div") el = new HTMLDivElement__();
    else if (tagName === "iframe") el = new HTMLIFrameElement__();
    else if (tagName === "script") el = new HTMLScriptElement__();
    else el = new Element__(tagName);
    Element__.call(el, tagName);
    return el;
  }

  // ===== Document =====
  var docLocation__ = {
    href: "https://duck.ai/",
    origin: __goOrigin,
    protocol: "https:",
    host: "duck.ai",
    hostname: "duck.ai",
    port: "",
    pathname: "/",
    search: "",
    hash: "",
  };

  function makeDocument__() {
    var docEl = new Element__("html");
    var head = new Element__("head");
    var body = new Element__("body");
    docEl.ownerDocument = docEl;
    head.ownerDocument = head;
    body.ownerDocument = body;
    docEl.appendChild(head);
    docEl.appendChild(body);

    var doc = {
      documentElement: docEl,
      head: head,
      body: body,
      cookie: "",
      title: "",
      referrer: "",
      URL: "https://duck.ai/",
      domain: "duck.ai",
      readyState: "complete",
      visibilityState: "visible",
      hidden: false,
      defaultView: null,
      __jsa__: null,
      location: docLocation__,
      createElement: function (tagName) { var el = createElement__(tagName); el.ownerDocument = this; return el; },
      createTextNode: function () { return {}; },
      createComment: function () { return {}; },
      createEvent: function () { return { initEvent: function () {} }; },
      dispatchEvent: function () { return true; },
      addEventListener: function () {},
      removeEventListener: function () {},
      querySelectorAll: function (selector) { return docEl.querySelectorAll(selector); },
      querySelector: function (selector) {
        return selector === "#jsa" && this.__jsa__ ? this.__jsa__ : docEl.querySelector(selector);
      },
      getElementById: function (id) {
        return id === "jsa" && this.__jsa__ ? this.__jsa__ : null;
      },
    };
    docEl.ownerDocument = doc;
    head.ownerDocument = doc;
    body.ownerDocument = doc;
    return doc;
  }

  // ===== Main document =====
  var doc__ = makeDocument__();

  // ===== Sandbox iframe =====
  var contentDoc__ = makeDocument__();
  // CSP meta
  var cspMeta__ = contentDoc__.createElement("meta");
  cspMeta__.setAttribute("http-equiv", "Content-Security-Policy");
  cspMeta__.setAttribute("content", "default-src 'none'; script-src 'unsafe-inline';");
  contentDoc__.head.appendChild(cspMeta__);

  // iframe element
  var jsaFrame__ = doc__.createElement("iframe");
  jsaFrame__.setAttribute("id", "jsa");
  jsaFrame__.setAttribute("sandbox", "allow-scripts allow-same-origin");
  jsaFrame__.style.cssText = "position: absolute; left: -9999px; top: -9999px;";
  jsaFrame__.srcdoc = "<!DOCTYPE html>\n<html>\n<head>\n<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'unsafe-inline';\">\n</head>\n<body></body>\n</html>";

  // Iframe content window — the challenge code runs here
  var contentWin__ = {
    Array: Array, Promise: Promise, Proxy: Proxy, Symbol: Symbol,
    Object: Object, JSON: JSON, Math: Math, Date: Date,
    String: String, Number: Number, Boolean: Boolean, RegExp: RegExp,
    Map: Map, Set: Set, WeakMap: WeakMap, WeakSet: WeakSet,
    Error: Error, TypeError: TypeError, RangeError: RangeError,
    ReferenceError: ReferenceError, SyntaxError: SyntaxError,
    EvalError: EvalError, URIError: URIError,
    Uint8Array: Uint8Array, Uint16Array: Uint16Array, Uint32Array: Uint32Array,
    Int8Array: Int8Array, Int16Array: Int16Array, Int32Array: Int32Array,
    Float32Array: Float32Array, Float64Array: Float64Array,
    ArrayBuffer: ArrayBuffer, DataView: DataView,
    TextEncoder: TextEncoder__,
    navigator: navigator__,
    document: contentDoc__,
    location: { href: "about:srcdoc", origin: "null", protocol: "about:", host: "", hostname: "", port: "", pathname: "srcdoc", search: "", hash: "" },
    btoa: function (v) { return __goBtoa(v); },
    atob: function (v) { return __goAtob(v); },
    setTimeout: function (fn) { if (typeof fn === "function") fn(); return 0; },
    clearTimeout: function () {},
    setInterval: function () { return 0; },
    clearInterval: function () {},
    addEventListener: function () {},
    removeEventListener: function () {},
    postMessage: function () {},
    getComputedStyle: function (el) { return el && el.style ? el.style : { getPropertyValue: function () { return ""; }, cssText: "" }; },
    screen: { width: 1920, height: 1080, availWidth: 1920, availHeight: 1040, colorDepth: 24, pixelDepth: 24 },
    crypto: { subtle: { digest: function () { return Promise.resolve(new ArrayBuffer(32)); } } },
    performance: { now: function () { var t = Date.now(); return t % 1000 + Math.random(); } },
    console: { log: function () {}, warn: function () {}, error: function () {}, info: function () {}, debug: function () {} },
    __jsaCallbacks__: {},
    // === Window identity checks ===
    constructor: function Window() {},
    navigator: navigator__,
  };
  contentWin__.self = contentWin__;
  contentWin__.window = contentWin__;
  contentWin__.top = globalThis;
  contentWin__.parent = globalThis;
  contentWin__[Symbol.toStringTag] = "Window";
  // Object.getOwnPropertyNames support for window property enumeration
  contentWin__.Window = function Window() {};
  contentWin__.Window.prototype = contentWin__;
  contentDoc__.defaultView = contentWin__;
  jsaFrame__.contentDocument = contentDoc__;
  jsaFrame__.contentWindow = contentWin__;

  doc__.body.appendChild(jsaFrame__);
  doc__.__jsa__ = jsaFrame__;

  // ===== Global property install =====
  function defProp__(obj, name, value) {
    Object.defineProperty(obj, name, { value: value, writable: true, configurable: true });
  }
  defProp__(globalThis, "window", globalThis);
  defProp__(globalThis, "self", globalThis);
  defProp__(globalThis, "top", globalThis);
  defProp__(globalThis, "document", doc__);
  defProp__(globalThis, "location", docLocation__);
  defProp__(globalThis, "navigator", navigator__);
  defProp__(globalThis, "TextEncoder", TextEncoder__);
  defProp__(globalThis, "Element", Element__);
  defProp__(globalThis, "HTMLElement", HTMLElement__);
  defProp__(globalThis, "HTMLDivElement", HTMLDivElement__);
  defProp__(globalThis, "HTMLIFrameElement", HTMLIFrameElement__);
  defProp__(globalThis, "HTMLScriptElement", HTMLScriptElement__);
  defProp__(globalThis, "NodeList", NodeList__);
  defProp__(globalThis, "__DDG_BE_VERSION__", "dev");
  defProp__(globalThis, "__DDG_FE_CHAT_HASH__", "hash");
  defProp__(globalThis, "Window", function Window() {});
  try { defProp__(globalThis.Window, "prototype", globalThis); } catch (e) {
    // Goja 的 Window 构造函数的 prototype 不可重新定义, 跳过
  }
  // Symbol.toStringTag: 让 Object.prototype.toString.call(window) === "[object Window]"
  if (typeof Symbol !== "undefined" && Symbol.toStringTag) {
    defProp__(globalThis, Symbol.toStringTag, "Window");
    defProp__(contentWin__, Symbol.toStringTag, "Window");
  }
  defProp__(globalThis, "btoa", function (v) { return __goBtoa(v); });
  defProp__(globalThis, "atob", function (v) { return __goAtob(v); });
  defProp__(globalThis, "getComputedStyle", function (el) {
    return el && el.style ? el.style : { getPropertyValue: function () { return ""; }, cssText: "" };
  });
  defProp__(globalThis, "setTimeout", function (fn) { if (typeof fn === "function") fn(); return 0; });
  defProp__(globalThis, "clearTimeout", function () {});
  defProp__(globalThis, "setInterval", function () { return 0; });
  defProp__(globalThis, "clearInterval", function () {});
  defProp__(globalThis, "performance", {
    now: function () { var t = Date.now(); return t % 1000 + Math.random(); },
    timing: { navigationStart: Date.now() - 1000 },
    memory: { jsHeapSizeLimit: 2172649472, totalJSHeapSize: 10000000, usedJSHeapSize: 8000000 },
    timeOrigin: Date.now() - 1000,
  });
  defProp__(globalThis, "crypto", {
    subtle: { digest: function () { return Promise.resolve(new ArrayBuffer(32)); } },
    getRandomValues: function (arr) {
      for (var i = 0; i < arr.length; i++) arr[i] = Math.floor(Math.random() * 256);
      return arr;
    },
  });
  defProp__(globalThis, "screen", { width: 1920, height: 1080, availWidth: 1920, availHeight: 1040, colorDepth: 24, pixelDepth: 24 });
  defProp__(globalThis, "history", { length: 1, state: null, scrollRestoration: "auto" });
  defProp__(globalThis, "localStorage", (function () {
    var s = {};
    return {
      getItem: function (k) { return s[k] !== undefined ? s[k] : null; },
      setItem: function (k, v) { s[String(k)] = String(v); },
      removeItem: function (k) { delete s[String(k)]; },
      clear: function () { s = {}; },
      get length() { return Object.keys(s).length; },
      key: function (i) { return Object.keys(s)[i] || null; },
    };
  })());
  defProp__(globalThis, "sessionStorage", (function () {
    var s = {};
    return {
      getItem: function (k) { return s[k] !== undefined ? s[k] : null; },
      setItem: function (k, v) { s[String(k)] = String(v); },
      removeItem: function (k) { delete s[String(k)]; },
      clear: function () { s = {}; },
      get length() { return Object.keys(s).length; },
      key: function (i) { return Object.keys(s)[i] || null; },
    };
  })());
  defProp__(globalThis, "console", {
    log: function () {}, warn: function () {}, error: function () {}, info: function () {}, debug: function () {},
  });
  defProp__(globalThis, "XMLHttpRequest", function () {
    this.open = function () {}; this.send = function () {}; this.setRequestHeader = function () {};
    this.abort = function () {}; this.readyState = 4; this.status = 200; this.responseText = "";
  });
  defProp__(globalThis, "fetch", function () { return Promise.resolve({ ok: true, status: 200, json: function () { return Promise.resolve({}); }, headers: { get: function () { return null; } } }); });
  defProp__(globalThis, "URL", function (url) {
    var u = { href: url, protocol: "https:", host: "", hostname: "", port: "", pathname: "/", search: "", hash: "", origin: __goOrigin };
    return u;
  });
  defProp__(globalThis, "URLSearchParams", function () { this.get = function () { return null; }; this.set = function () {}; this.keys = function () { return []; }; });
  defProp__(globalThis, "requestAnimationFrame", function (fn) { if (typeof fn === "function") fn(0); return 0; });
  defProp__(globalThis, "cancelAnimationFrame", function () {});
  defProp__(globalThis, "matchMedia", function () { return { matches: false, addListener: function () {}, removeListener: function () {}, addEventListener: function () {}, removeEventListener: function () {} }; });
  defProp__(globalThis, "ResizeObserver", function () { this.observe = function () {}; this.disconnect = function () {}; this.unobserve = function () {}; });
  defProp__(globalThis, "IntersectionObserver", function () { this.observe = function () {}; this.disconnect = function () {}; this.unobserve = function () {}; });
  defProp__(globalThis, "MutationObserver", function () { this.observe = function () {}; this.disconnect = function () {}; this.takeRecords = function () { return []; }; });
  defProp__(globalThis, "Image", function () {
    var img = { width: 0, height: 0, src: "", onload: null, onerror: null, naturalWidth: 0, naturalHeight: 0, complete: false };
    return img;
  });

  // Fix constructor .name for challenge compatibility
  try { Object.defineProperty(NodeList, "name", { value: "NodeList", configurable: true }); } catch (e) {}
  try { Object.defineProperty(Element, "name", { value: "Element", configurable: true }); } catch (e) {}
  try { Object.defineProperty(HTMLElement, "name", { value: "HTMLElement", configurable: true }); } catch (e) {}
  try { Object.defineProperty(HTMLDivElement, "name", { value: "HTMLDivElement", configurable: true }); } catch (e) {}
  try { Object.defineProperty(HTMLIFrameElement, "name", { value: "HTMLIFrameElement", configurable: true }); } catch (e) {}
  try { Object.defineProperty(HTMLScriptElement, "name", { value: "HTMLScriptElement", configurable: true }); } catch (e) {}
})();
