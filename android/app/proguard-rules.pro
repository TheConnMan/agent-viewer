# kotlinx.serialization: keep the plugin-generated serializers and the classes they decode into.
-keepattributes *Annotation*, InnerClasses
-dontnote kotlinx.serialization.AnnotationsKt
-keepclassmembers class kotlinx.serialization.json.** { *** Companion; }
-keepclasseswithmembers class kotlinx.serialization.json.** { kotlinx.serialization.KSerializer serializer(...); }
-keep,includedescriptorclasses class com.theconnman.agentviewer.**$$serializer { *; }
-keepclassmembers class com.theconnman.agentviewer.** { *** Companion; }
-keepclasseswithmembers class com.theconnman.agentviewer.** { kotlinx.serialization.KSerializer serializer(...); }
# Vendored Termux emulator: reflection-free, but keep names so stack traces stay readable.
-keep class com.termux.** { *; }
