plugins { id("com.android.library") }
android {
    namespace = "com.termux.view"
    compileSdk = 35
    defaultConfig { minSdk = 29 }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
    testOptions { unitTests { isIncludeAndroidResources = true; isReturnDefaultValues = false } }
}
dependencies {
    implementation("androidx.annotation:annotation:1.9.1")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.14.1")
}
tasks.withType<Test> { testLogging { showStandardStreams = true; events("passed", "failed") } }
