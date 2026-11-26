package net.christopherrutherford.awesometv;

import android.annotation.SuppressLint;
import android.app.Activity;
import android.content.SharedPreferences;
import android.net.Uri;
import android.graphics.Color;
import android.os.Build;
import android.os.Bundle;
import android.text.InputType;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowManager;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputMethodManager;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.TextView;

public class MainActivity extends Activity {
    private static final String PREFS = "channelloom";
    private static final String PREF_SERVER_URL = "server_url";

    private FrameLayout root;
    private View overlay;
    private String serverUrl;
    private WebView webView;
    private View customView;
    private WebChromeClient.CustomViewCallback customViewCallback;

    @SuppressLint("SetJavaScriptEnabled")
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        hideSystemUi();

        root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        webView = new WebView(this);
        webView.setBackgroundColor(Color.BLACK);
        webView.setFocusable(true);
        webView.requestFocus();

        WebSettings settings = webView.getSettings();
        settings.setJavaScriptEnabled(true);
        settings.setDomStorageEnabled(true);
        settings.setDatabaseEnabled(true);
        settings.setMediaPlaybackRequiresUserGesture(false);
        settings.setLoadWithOverviewMode(true);
        settings.setUseWideViewPort(true);
        settings.setMixedContentMode(WebSettings.MIXED_CONTENT_COMPATIBILITY_MODE);

        if (BuildConfig.DEBUG) {
            WebView.setWebContentsDebuggingEnabled(true);
        }

        webView.setWebViewClient(new WebViewClient() {
            @Override
            public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
                return false;
            }

            @Override
            public void onReceivedError(WebView view, WebResourceRequest request, WebResourceError error) {
                if (request.isForMainFrame()) {
                    showError("Cannot reach ChannelLoom at\n" + request.getUrl());
                }
            }
        });

        webView.setWebChromeClient(new WebChromeClient() {
            @Override
            public void onShowCustomView(View view, CustomViewCallback callback) {
                if (customView != null) {
                    callback.onCustomViewHidden();
                    return;
                }
                customView = view;
                customViewCallback = callback;
                root.addView(customView, new FrameLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT));
                webView.setVisibility(View.GONE);
                hideSystemUi();
            }

            @Override
            public void onHideCustomView() {
                if (customView != null) {
                    root.removeView(customView);
                    customView = null;
                }
                if (customViewCallback != null) {
                    customViewCallback.onCustomViewHidden();
                    customViewCallback = null;
                }
                webView.setVisibility(View.VISIBLE);
                webView.requestFocus();
                hideSystemUi();
            }
        });

        root.addView(webView, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT));
        setContentView(root);

        // An explicit `url` extra (adb shell am start ... --es url ...) wins,
        // then the saved server, then the build-time default if one was set.
        String url = normalizeServerUrl(getIntent().getStringExtra("url"));
        if (url == null) url = normalizeServerUrl(prefs().getString(PREF_SERVER_URL, null));
        if (url == null) url = normalizeServerUrl(BuildConfig.APP_URL);
        if (url == null) {
            showSetup(null);
        } else {
            connect(url);
        }
    }

    private SharedPreferences prefs() {
        return getSharedPreferences(PREFS, MODE_PRIVATE);
    }

    private void connect(String url) {
        serverUrl = url;
        InputMethodManager ime = (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
        if (ime != null) ime.hideSoftInputFromWindow(root.getWindowToken(), 0);
        clearOverlay();
        webView.setVisibility(View.VISIBLE);
        webView.requestFocus();
        webView.loadUrl(url);
    }

    /** Accepts "host", "host:port" or a full URL; returns null if unusable. */
    static String normalizeServerUrl(String input) {
        if (input == null) return null;
        String value = input.trim();
        if (value.isEmpty()) return null;
        if (!value.contains("://")) value = "http://" + value;
        Uri uri = Uri.parse(value);
        String scheme = uri.getScheme();
        if (scheme == null || !(scheme.equalsIgnoreCase("http") || scheme.equalsIgnoreCase("https"))) {
            return null;
        }
        String host = uri.getHost();
        if (host == null || host.isEmpty()) return null;
        String path = uri.getEncodedPath();
        if (path == null || path.isEmpty()) value = value + "/";
        return value;
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent event) {
        if (overlay != null) {
            return super.dispatchKeyEvent(event);
        }
        if (event.getAction() == KeyEvent.ACTION_DOWN && webView != null) {
            if (event.getKeyCode() == KeyEvent.KEYCODE_MENU
                    || event.getKeyCode() == KeyEvent.KEYCODE_SETTINGS) {
                showSetup(null);
                return true;
            }
            String key = null;
            switch (event.getKeyCode()) {
                case KeyEvent.KEYCODE_DPAD_LEFT:
                    key = "ArrowLeft";
                    break;
                case KeyEvent.KEYCODE_DPAD_RIGHT:
                    key = "ArrowRight";
                    break;
                case KeyEvent.KEYCODE_DPAD_UP:
                    key = "ArrowUp";
                    break;
                case KeyEvent.KEYCODE_DPAD_DOWN:
                    key = "ArrowDown";
                    break;
                case KeyEvent.KEYCODE_DPAD_CENTER:
                case KeyEvent.KEYCODE_ENTER:
                    key = "Enter";
                    break;
                case KeyEvent.KEYCODE_BACK:
                    if (webView.canGoBack()) {
                        webView.goBack();
                        return true;
                    }
                    break;
                default:
                    break;
            }
            if (key != null) {
                String js = "document.dispatchEvent(new KeyboardEvent('keydown',{key:'"
                        + key + "',bubbles:true,cancelable:true}))";
                webView.evaluateJavascript(js, null);
                return true;
            }
        }
        return super.dispatchKeyEvent(event);
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            hideSystemUi();
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (webView != null) webView.onResume();
        hideSystemUi();
    }

    @Override
    protected void onPause() {
        if (webView != null) webView.onPause();
        super.onPause();
    }

    @Override
    protected void onDestroy() {
        if (webView != null) {
            webView.loadUrl("about:blank");
            webView.destroy();
            webView = null;
        }
        super.onDestroy();
    }

    private void hideSystemUi() {
        View decor = getWindow().getDecorView();
        decor.setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
    }

    private void showError(String message) {
        LinearLayout panel = panel();
        panel.addView(text(message, 20));
        Button retry = button("Retry");
        retry.setOnClickListener(v -> connect(serverUrl));
        Button change = button("Change server");
        change.setOnClickListener(v -> showSetup(null));
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER);
        row.addView(retry);
        row.addView(change);
        panel.addView(row);
        showOverlay(panel);
        retry.requestFocus();
    }

    private void showSetup(String problem) {
        LinearLayout panel = panel();
        panel.addView(text("Connect to your ChannelLoom server", 26));
        panel.addView(text("Enter the address shown when the server starts, "
                + "for example http://192.168.1.20:8409", 16));
        EditText input = new EditText(this);
        input.setSingleLine(true);
        input.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        input.setImeOptions(EditorInfo.IME_ACTION_GO | EditorInfo.IME_FLAG_NO_EXTRACT_UI);
        input.setTextColor(Color.WHITE);
        input.setHintTextColor(Color.GRAY);
        input.setHint("http://192.168.1.20:8409");
        input.setMinWidth(720);
        String current = serverUrl != null ? serverUrl : prefs().getString(PREF_SERVER_URL, "");
        input.setText(current);
        input.setSelection(input.getText().length());
        TextView status = text(problem == null ? "" : problem, 16);
        status.setTextColor(Color.rgb(0xFF, 0x8A, 0x80));
        panel.addView(status);
        panel.addView(input);
        Button go = button("Connect");
        Runnable submit = () -> {
            String url = normalizeServerUrl(input.getText().toString());
            if (url == null) {
                status.setText("Enter an http:// or https:// address.");
                return;
            }
            prefs().edit().putString(PREF_SERVER_URL, url).apply();
            connect(url);
        };
        go.setOnClickListener(v -> submit.run());
        input.setOnEditorActionListener((v, actionId, event) -> {
            submit.run();
            return true;
        });
        panel.addView(go);
        if (serverUrl != null) {
            Button cancel = button("Cancel");
            cancel.setOnClickListener(v -> connect(serverUrl));
            panel.addView(cancel);
        }
        showOverlay(panel);
        input.requestFocus();
    }

    private LinearLayout panel() {
        LinearLayout panel = new LinearLayout(this);
        panel.setOrientation(LinearLayout.VERTICAL);
        panel.setGravity(Gravity.CENTER);
        panel.setPadding(64, 48, 64, 48);
        panel.setBackgroundColor(Color.rgb(0x10, 0x16, 0x2B));
        return panel;
    }

    private TextView text(String value, int sizeSp) {
        TextView view = new TextView(this);
        view.setTextColor(Color.WHITE);
        view.setTextSize(sizeSp);
        view.setGravity(Gravity.CENTER);
        view.setPadding(0, 12, 0, 12);
        view.setText(value);
        return view;
    }

    private Button button(String label) {
        Button button = new Button(this);
        button.setText(label);
        button.setFocusable(true);
        return button;
    }

    private void showOverlay(View view) {
        clearOverlay();
        overlay = view;
        webView.setVisibility(View.GONE);
        root.addView(view, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.MATCH_PARENT));
    }

    private void clearOverlay() {
        if (overlay != null) {
            root.removeView(overlay);
            overlay = null;
        }
    }
}
