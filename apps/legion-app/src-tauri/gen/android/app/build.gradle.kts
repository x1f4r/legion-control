import java.time.ZoneOffset
import java.time.ZonedDateTime
import java.util.Properties
import org.gradle.api.GradleException
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// Mirrors `legion_update::android_version_code` and `scripts/make-update-set.mjs` so Android's own
// downgrade check always agrees with Legion's update sequence.
fun androidVersionCodeFromSequence(raw: String): Int {
    val seq = raw.trim().toLongOrNull()
        ?: throw GradleException("LEGION_BUILD_SEQUENCE='$raw' is not a positive integer.")
    if (seq <= 0L) {
        throw GradleException("LEGION_BUILD_SEQUENCE='$raw' must be greater than 0.")
    }
    if (seq <= 20_000_000L) {
        return seq.toInt()
    }
    if (seq in 202001010000L..299912312359L) {
        val minute = (seq % 100L).toInt()
        val hour = ((seq / 100L) % 100L).toInt()
        val day = ((seq / 10_000L) % 100L).toInt()
        val month = ((seq / 1_000_000L) % 100L).toInt()
        val year = (seq / 100_000_000L).toInt()
        val dt = try {
            ZonedDateTime.of(year, month, day, hour, minute, 0, 0, ZoneOffset.UTC)
        } catch (e: Exception) {
            throw GradleException(
                "LEGION_BUILD_SEQUENCE='$raw' is not a valid UTC YYYYMMDDHHMM timestamp: ${e.message}"
            )
        }
        val unixMinutes = dt.toEpochSecond() / 60L
        if (unixMinutes <= 0L || unixMinutes > 2_100_000_000L) {
            throw GradleException("LEGION_BUILD_SEQUENCE='$raw' overflows Android versionCode.")
        }
        return unixMinutes.toInt()
    }
    throw GradleException(
        "LEGION_BUILD_SEQUENCE='$raw' cannot map to Android versionCode (pass UTC YYYYMMDDHHMM or 1..20000000)."
    )
}

val buildSequenceEnv = System.getenv("LEGION_BUILD_SEQUENCE")?.trim()?.takeIf { it.isNotEmpty() }
val resolvedVersionCode = if (buildSequenceEnv != null) {
    androidVersionCodeFromSequence(buildSequenceEnv)
} else {
    tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
}

android {
    compileSdk = 37
    namespace = "com.x1f4r.legion"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.x1f4r.legion"
        minSdk = 30
        targetSdk = 37
        versionCode = resolvedVersionCode
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    buildTypes {
        getByName("debug") {
            applicationIdSuffix = ".debug"
            manifestPlaceholders["usesCleartextTraffic"] = "false"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            optimization {
                enable = true
            }
            isShrinkResources = true
            proguardFiles(
                *fileTree(".") {
                    include("**/*.pro")
                    exclude("build/**")
                }.files.toTypedArray()
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    buildFeatures {
        buildConfig = true
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = file("tauri.build.gradle.kts"))
