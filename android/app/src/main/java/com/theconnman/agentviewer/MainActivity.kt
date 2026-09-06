package com.theconnman.agentviewer

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // Edge to edge on every supported API level, not only where the platform forces it, so
        // the app paints behind the status bar itself and the bar icons follow the theme.
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContent { AgentViewerApp() }
    }
}
