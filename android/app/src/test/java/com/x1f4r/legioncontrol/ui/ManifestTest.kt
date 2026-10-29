package com.x1f4r.legioncontrol.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

/**
 * What the manifest promises about the app's lifecycle, held to the code that depends on it (A1, A6).
 *
 * None of this fails at build time when it is wrong. A configuration change the activity does not
 * absorb recreates it and throws away what is half typed; a foreground service whose type has no
 * permission behind it is refused on Android 14 and later, and the change it was keeping alive goes
 * back to being frozen with the phone; an application class nobody names is never built, and the
 * one copy of every store with it.
 */
class ManifestTest {

    private val manifest: Element = run {
        val path = System.getProperty("legion.manifest")
            ?: error("legion.manifest is not set; run the tests through Gradle")
        DocumentBuilderFactory.newInstance()
            .apply { isNamespaceAware = true }
            .newDocumentBuilder()
            .parse(File(path))
            .documentElement
    }

    private fun Element.android(name: String): String = getAttributeNS(ANDROID, name)

    private fun elements(tag: String): List<Element> {
        val nodes = manifest.getElementsByTagName(tag)
        return (0 until nodes.length).map { nodes.item(it) as Element }
    }

    private fun component(tag: String, name: String): Element =
        elements(tag).singleOrNull { it.android("name") == name } ?: error("the manifest declares no $tag $name")

    private val permissions: Set<String>
        get() = elements("uses-permission").map { it.android("name") }.toSet()

    @Test
    fun `the models belong to an application the manifest names`() {
        assertEquals(LegionApplication::class.java.name, elements("application").single().android("name"))
    }

    @Test
    fun `the activity absorbs every configuration change a person makes without meaning to`() {
        val absorbed = component("activity", MainActivity::class.java.name).android("configChanges").split('|').toSet()
        val wanted = setOf(
            "orientation", "screenSize", "smallestScreenSize", "screenLayout", "keyboardHidden", "uiMode",
            "density", "fontScale", "locale", "layoutDirection",
        )
        assertTrue("missing ${wanted - absorbed}", absorbed.containsAll(wanted))
    }

    @Test
    fun `the service that keeps a change running is private and declared for what it does`() {
        val service = component("service", ChangeService::class.java.name)
        assertEquals("false", service.android("exported"))
        assertEquals("dataSync", service.android("foregroundServiceType"))
        assertTrue(
            "Android 14 and later refuse the type without its permission",
            permissions.containsAll(
                setOf("android.permission.FOREGROUND_SERVICE", "android.permission.FOREGROUND_SERVICE_DATA_SYNC"),
            ),
        )
    }

    private companion object {
        const val ANDROID = "http://schemas.android.com/apk/res/android"
    }
}
