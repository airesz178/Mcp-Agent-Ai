// Runs synchronously in <head>, before the stylesheet and deferred application.
// Avoid showing the desktop welcome layout during the first Android WebView paint.
if (/Android/i.test(navigator.userAgent)) {
  document.documentElement.classList.add('native-android');
}
