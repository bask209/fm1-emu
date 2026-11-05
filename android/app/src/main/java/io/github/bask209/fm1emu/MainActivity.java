package io.github.bask209.fm1emu;

import android.app.NativeActivity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.media.midi.MidiDevice;
import android.media.midi.MidiDeviceInfo;
import android.media.midi.MidiManager;
import android.media.midi.MidiOutputPort;
import android.media.midi.MidiReceiver;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.OpenableColumns;
import android.util.Log;
import android.view.View;
import android.view.WindowManager;

import java.io.File;
import java.io.FileOutputStream;
import java.io.FileWriter;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.Writer;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * The FM-1 emulator's activity: everything is native (libfm1_android.so, crates/fm1-android)
 * but what only Java reaches: the file picker and the MIDI devices.
 *
 * The picker's file is copied to files/firmware.new (its name to firmware.new.name), which
 * the native side watches for and boots. MIDI devices with an output port (a USB keyboard
 * such as the MPK mini 3) are opened as they come; their bytes go to nativeMidi.
 */
public class MainActivity extends NativeActivity {
    static {
        // (NativeActivity loads it too; loaded here first, its JNI methods resolve)
        System.loadLibrary("fm1_android");
    }

    static native void nativeMidi(int b);
    static native void nativeMidiDevice(String name);

    private static final String TAG = "fm1";
    private static final int PICK = 7;
    private MidiManager midi;
    private final List<MidiDevice> devices = new ArrayList<>();
    private final Set<Integer> opened = new HashSet<>();

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        immersive();
        startMidi();
        fromIntent(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        fromIntent(intent);
    }

    @Override
    public void onWindowFocusChanged(boolean focus) {
        super.onWindowFocusChanged(focus);
        if (focus) immersive();
    }

    @SuppressWarnings("deprecation")
    private void immersive() {
        getWindow().getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN);
    }

    /** Called from native code (any thread): let the user pick a firmware package. */
    public void pickFirmware() {
        runOnUiThread(() -> {
            Intent i = new Intent(Intent.ACTION_OPEN_DOCUMENT);
            i.addCategory(Intent.CATEGORY_OPENABLE);
            i.setType("*/*");
            try {
                startActivityForResult(i, PICK);
            } catch (Exception e) {
                Log.e(TAG, "no file picker: " + e);
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == PICK && result == RESULT_OK && data != null && data.getData() != null) {
            copyIn(data.getData());
        }
    }

    /** A .fwsc opened with this app from a file manager or a download. */
    private void fromIntent(Intent intent) {
        if (intent != null && Intent.ACTION_VIEW.equals(intent.getAction()) && intent.getData() != null) {
            copyIn(intent.getData());
        }
    }

    private void copyIn(final Uri uri) {
        new Thread(() -> {
            String name = "firmware.fwsc";
            try (Cursor c = getContentResolver().query(uri, null, null, null, null)) {
                if (c != null && c.moveToFirst()) {
                    int k = c.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                    if (k >= 0 && c.getString(k) != null) name = c.getString(k);
                }
            } catch (Exception e) {
                Log.w(TAG, "name of " + uri + ": " + e);
            }
            File dir = getFilesDir();
            File tmp = new File(dir, "firmware.tmp");
            long n = 0;
            try (InputStream in = getContentResolver().openInputStream(uri);
                 OutputStream out = new FileOutputStream(tmp)) {
                byte[] buf = new byte[1 << 16];
                int k;
                while ((k = in.read(buf)) > 0) {
                    out.write(buf, 0, k);
                    n += k;
                }
            } catch (Exception e) {
                Log.e(TAG, "cannot read " + uri + ": " + e);
                return;
            }
            try (Writer w = new FileWriter(new File(dir, "firmware.new.name"))) {
                w.write(name);
            } catch (Exception e) {
                Log.w(TAG, "firmware.new.name: " + e);
            }
            if (!tmp.renameTo(new File(dir, "firmware.new"))) {
                Log.e(TAG, "cannot rename " + tmp);
                return;
            }
            Log.i(TAG, "picked " + name + " (" + n + " bytes)");
        }).start();
    }

    // ------------------------------------------------------------------ MIDI in

    private void startMidi() {
        if (Build.VERSION.SDK_INT < 23 || !getPackageManager().hasSystemFeature(PackageManager.FEATURE_MIDI)) {
            Log.i(TAG, "MIDI: not on this device");
            return;
        }
        midi = (MidiManager) getSystemService(MIDI_SERVICE);
        if (midi == null) return;
        final Handler h = new Handler(Looper.getMainLooper());
        @SuppressWarnings("deprecation")
        MidiDeviceInfo[] infos = midi.getDevices();
        for (MidiDeviceInfo info : infos) openMidi(info, h);
        midi.registerDeviceCallback(new MidiManager.DeviceCallback() {
            @Override
            public void onDeviceAdded(MidiDeviceInfo info) {
                openMidi(info, h);
            }

            @Override
            public void onDeviceRemoved(MidiDeviceInfo info) {
                opened.remove(info.getId());
            }
        }, h);
    }

    private void openMidi(final MidiDeviceInfo info, Handler h) {
        if (info.getOutputPortCount() == 0 || opened.contains(info.getId())) return;
        opened.add(info.getId());
        midi.openDevice(info, dev -> {
            if (dev == null) {
                Log.w(TAG, "MIDI: cannot open " + info);
                opened.remove(info.getId());
                return;
            }
            devices.add(dev);
            Bundle p = info.getProperties();
            String name = p.getString(MidiDeviceInfo.PROPERTY_PRODUCT);
            if (name == null) name = p.getString(MidiDeviceInfo.PROPERTY_NAME);
            if (name == null) name = "MIDI device";
            nativeMidiDevice(name);
            for (int i = 0; i < info.getOutputPortCount(); i++) {
                MidiOutputPort port = dev.openOutputPort(i);
                if (port == null) continue;
                port.connect(new MidiReceiver() {
                    @Override
                    public void onSend(byte[] msg, int offset, int count, long timestamp) {
                        synchronized (MainActivity.class) {
                            for (int k = 0; k < count; k++) nativeMidi(msg[offset + k] & 0xff);
                        }
                    }
                });
            }
        }, h);
    }

    @Override
    protected void onDestroy() {
        for (MidiDevice d : devices) {
            try {
                d.close();
            } catch (Exception e) {
                // gone already
            }
        }
        super.onDestroy();
    }
}
