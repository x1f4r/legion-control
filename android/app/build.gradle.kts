plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
}

android {
    namespace = "com.x1f4r.legioncontrol"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.x1f4r.legioncontrol"
        minSdk = 30
        targetSdk = 36
        versionCode = 12
        versionName = "1.3.2"

        // The label lives here rather than in res/values/strings.xml so that the one string the
        // manifest needs cannot collide with the string table the UI owns.
        resValue("string", "app_name", "Legion Control")

        // A phone is arm64; the other ABIs only add weight to an APK that is installed by hand on
        // one device. Override with -Plegion.abiFilters=arm64-v8a,x86_64 for anything else.
        val abis = providers.gradleProperty("legion.abiFilters").orNull
            ?.split(',')
            ?.map { it.trim() }
            ?.filter { it.isNotEmpty() }
            ?: listOf("arm64-v8a")
        ndk { abiFilters += abis }
    }

    buildTypes {
        release {
            // R8 stays off. sshj picks its ciphers, key exchanges, MACs and signature implementations
            // out of name-keyed factory lists, so shrinking silently removes algorithms and the
            // failure only shows up as a handshake that cannot agree on anything. The app is a
            // handful of screens; the size saved is not worth debugging that on a phone.
            isMinifyEnabled = false
            isShrinkResources = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // No signingConfig on purpose: android/build-apk.sh signs the unsigned output with the
            // key it keeps outside the repository.
        }
        debug {
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
        // sshj and Bouncy Castle reach for pieces of the JDK that API 30 does not have all of.
        isCoreLibraryDesugaringEnabled = true
    }

    kotlin {
        compilerOptions {
            jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_11)
        }
    }

    buildFeatures {
        compose = true
        // AppUpdates compares the running versionName against the newest GitHub release.
        buildConfig = true
    }

    packaging {
        resources {
            // Bouncy Castle and sshj both ship signatures, service files and licence text that
            // collide once they are merged into one APK.
            excludes += setOf(
                "META-INF/DEPENDENCIES",
                "META-INF/LICENSE",
                "META-INF/LICENSE.txt",
                "META-INF/LICENSE.md",
                "META-INF/LICENSE-notice.md",
                "META-INF/NOTICE",
                "META-INF/NOTICE.txt",
                "META-INF/NOTICE.md",
                "META-INF/*.kotlin_module",
                "META-INF/versions/9/OSGI-INF/MANIFEST.MF",
                "META-INF/INDEX.LIST",
                "META-INF/*.SF",
                "META-INF/*.DSA",
                "META-INF/*.RSA",
                // Bouncy Castle's post-quantum lookup tables, a megabyte of them. ssh does not have a
                // post-quantum key exchange in this version of sshj and never reaches this code.
                "org/bouncycastle/pqc/**",
            )
        }
    }

    // The signed agent bundle, copied out of the repository's dist/ directory at build time.
    //
    // Copied rather than committed: it is a build output of scripts/package-agent.mjs, it is signed
    // by the release key, and a client that carried its own copy in source control would be one more
    // place for the two to drift apart. A build without it is normal and expected: every test and
    // every debug build works, and the "install the control agent" action explains that this build
    // carries nothing to install rather than offering something unverified.
    sourceSets["main"].assets.srcDir(layout.buildDirectory.dir("generated/agentAssets"))

    lint {
        // The manifest names an activity the UI module supplies, so the class-existence check has
        // nothing to look at during a transport-only build.
        disable += "MissingClass"
    }
}

/**
 * Puts the signed agent bundle where the app reads it, when there is one.
 *
 * The three names are the contract's exact basenames. Nothing here verifies the signature: that
 * happens in the app, at run time, against the key pinned in Trust.kt, because a check performed by
 * the thing doing the copying proves nothing about the thing doing the installing.
 */
val copyAgentBundle by tasks.registering(Sync::class) {
    val dist = rootProject.layout.projectDirectory.dir("../dist")
    val version = providers.gradleProperty("legion.agentVersion").getOrElse("3.0.0")
    from(dist) {
        include("legionctl-agent-$version.tgz")
        include("Legion-Control-agent-manifest.json")
        include("Legion-Control-agent-manifest.json.sig")
    }
    into(layout.buildDirectory.dir("generated/agentAssets/agent"))
    // Sync also removes a previously bundled artifact when dist no longer supplies it.
}

tasks.named("preBuild") { dependsOn(copyAgentBundle) }

dependencies {
    coreLibraryDesugaring(libs.desugar.jdk.libs)

    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.activity.compose)

    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    // material3 brings material-icons-core with it, which is the few hundred icons anyone actually
    // uses. material-icons-extended is deliberately not here: it is every Material icon ever drawn,
    // it added 60 MB to the APK, and R8 is off so none of it would ever be shrunk away again.
    implementation(libs.androidx.compose.material3)
    debugImplementation(libs.androidx.compose.ui.tooling)

    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.kotlinx.serialization.json)

    testImplementation(libs.junit)

    implementation(libs.sshj)
    implementation(libs.eddsa)
    // sshj logs through slf4j. Without a binding, slf4j 2 falls back to a no-op and prints a warning
    // on first use; this sends the same lines to logcat instead, which is where they are useful.
    runtimeOnly(libs.slf4j.android)
}

// Shared contract and signature fixtures are test inputs even though they live outside Android.
tasks.withType<org.gradle.api.tasks.testing.Test>().configureEach {
    inputs.files(rootProject.fileTree("../contract/fixtures") { include("*.json") })
    inputs.files(rootProject.file("../contract/hash-vectors.json"))
    inputs.files(rootProject.file("../contract/release-public-key.pem"))
    inputs.files(rootProject.fileTree("../tests/fixtures/trust") { include("*") })
}
